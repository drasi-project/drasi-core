# Profiler Reaction

## Purpose

The Profiler reaction collects and analyzes performance metrics for query result processing. It provides real-time statistical analysis of latencies across the entire data pipeline, helping identify performance bottlenecks and monitor system health.

## Metrics Collected

The profiler tracks five key latency metrics:

1. **Source→Query**: Channel latency from source to query component (indicates backpressure)
2. **Query Processing**: Time spent in drasi-core query evaluation
3. **Query→Reaction**: Channel latency from query to reaction (indicates backpressure)
4. **Reaction Processing**: Time spent in reaction processing (HTTP calls, DB writes, etc.)
5. **Total End-to-End**: Complete pipeline latency from source send to reaction complete

### Statistics Reported

For each metric, the profiler reports:
- **count**: Total samples collected
- **mean**: Average latency in milliseconds
- **stddev**: Standard deviation (measure of variability)
- **min/max**: Minimum and maximum observed latencies
- **p50**: Median latency (50th percentile)
- **p95**: 95th percentile (95% of samples are faster)
- **p99**: 99th percentile (tail latency indicator)

## Statistical Methods

### Welford's Algorithm

The profiler uses **Welford's algorithm** for computing mean and variance in a single pass:

**Benefits**:
- Constant O(1) memory usage regardless of sample count
- Numerically stable (avoids catastrophic cancellation)
- Online updates without reprocessing previous samples

**Formula**:
```
count += 1
delta = value - mean
mean += delta / count
delta2 = value - mean
m2 += delta * delta2
variance = m2 / (count - 1)  // Sample variance (Bessel's correction)
```

### Percentile Calculation

Percentiles are computed from a sliding window of recent samples:
1. Store recent samples in a VecDeque (circular buffer)
2. Extract and sort values for the metric
3. Calculate percentile using linear interpolation: `index = round(p × (len - 1))`

**Note**: Percentiles reflect the sliding window only, while mean and variance include all samples since startup.

## Configuration

### Properties

- **window_size** (default: 1000): Number of recent samples retained for percentile calculation
  - Small (100-500): Recent trends, responsive to changes
  - Medium (1000-5000): Balanced stability (recommended)
  - Large (5000-10000): Very stable statistics, higher memory

- **report_interval_secs** (default: 10): How often to log statistical reports
  - Short (5-15s): Active debugging and development
  - Medium (30-60s): Production monitoring
  - Long (120-300s): Low-overhead monitoring

### Configuration Example

**YAML**:
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

**Rust**:
```rust
use drasi_server_core::config::ReactionConfig;

let config = ReactionConfig {
    id: "profiler".to_string(),
    reaction_type: "profiler".to_string(),
    queries: vec!["my-query".to_string()],
    auto_start: true,
    properties: {
        let mut props = HashMap::new();
        props.insert("window_size".to_string(), serde_json::json!(1000));
        props.insert("report_interval_secs".to_string(), serde_json::json!(30));
        props
    },
};
```

## Output Format

### Example Report

```
INFO [profiler] ========== Profiling Report ==========
INFO [profiler] Source→Query: mean=0.24ms, stddev=0.08ms, min=0.12ms, p50=0.23ms, p95=0.39ms, p99=0.52ms, max=0.89ms (n=1000)
INFO [profiler] Query Processing: mean=1.15ms, stddev=0.35ms, min=0.45ms, p50=1.10ms, p95=1.85ms, p99=2.30ms, max=3.75ms (n=1000)
INFO [profiler] Query→Reaction: mean=0.16ms, stddev=0.04ms, min=0.10ms, p50=0.15ms, p95=0.23ms, p99=0.31ms, max=0.45ms (n=1000)
INFO [profiler] Reaction Processing: mean=0.85ms, stddev=0.21ms, min=0.40ms, p50=0.82ms, p95=1.25ms, p99=1.65ms, max=2.10ms (n=1000)
INFO [profiler] Total End-to-End: mean=2.48ms, stddev=0.62ms, min=1.18ms, p50=2.41ms, p95=3.58ms, p99=4.18ms, max=5.28ms (n=1000)
INFO [profiler] ======================================
```

## Interpreting Results

### Identifying Bottlenecks

Compare metrics to identify where delays occur:

| Highest Metric | Bottleneck | Solution |
|----------------|------------|----------|
| Query Processing | Complex query | Optimize Cypher query, add indexes |
| Reaction Processing | Slow external calls | Optimize reaction (batching, caching) |
| Source→Query | Query backpressure | Scale query processing |
| Query→Reaction | Reaction backpressure | Scale reaction, increase concurrency |

### Using Percentiles for SLA Monitoring

**Why percentiles matter more than averages**:
```
Mean: 10ms    ← Looks great!
p50:  5ms     ← Most requests are fast
p95:  15ms    ← Still acceptable
p99:  500ms   ← 1% of users have terrible experience!
```

**Typical SLA targets**:
- p50 < 10ms: Most requests are very fast
- p95 < 50ms: 95% of users have good experience
- p99 < 200ms: Even tail latency is acceptable

### Normal vs Warning Ranges

**Normal ranges** (typical production):
- Source→Query: 0.1-1ms
- Query Processing: 1-50ms (depends on query complexity)
- Query→Reaction: 0.1-1ms
- Reaction Processing: 1-100ms (depends on reaction type)
- Total End-to-End: 5-150ms

**Warning signs**:
- Any channel latency > 10ms → Backpressure detected
- Query processing > 100ms → Complex query needs optimization
- Reaction processing > 500ms → Slow external calls
- Total latency > 1000ms → System under stress

## Performance Impact

### CPU Overhead

- Welford's algorithm: O(1) per sample, ~50-100 CPU cycles
- Percentile calculation: O(N log N) per report (N = window_size)
- Normal workloads: < 0.01% CPU overhead
- High event rates (>100K/sec): ~0.1-1% CPU overhead

### Memory Usage

- Per sample: ~72 bytes (ProfilingMetadata struct)
- Total: window_size × 72 bytes
- Examples:
  - window_size = 1,000 → ~72 KB
  - window_size = 10,000 → ~720 KB

**Recommendations**:
- Use window_size < 10,000 for most cases
- Monitor memory if using window_size > 10,000
- Increase report_interval_secs to reduce report overhead

## Enabling Profiling

### Source Configuration

Enable profiling in your source configuration:

```yaml
sources:
  - id: my-source
    source_type: postgres
    properties:
      enable_profiling: true
      # ... other source properties
```

### Profiling Data Flow

```
1. Source: Sets source timestamps
   ↓ (ProfilingMetadata flows in SourceEventWrapper)

2. Query: Adds query timestamps
   ↓ (ProfilingMetadata flows in QueryResult)

3. Profiler Reaction: Collects, analyzes, reports statistics
```

## Troubleshooting

### Missing Profiling Data

**Problem**: Profiler logs "No profiling data collected yet"

**Solutions**:
1. Verify source has `enable_profiling: true`
2. Check query is receiving events
3. Verify profiler is subscribed to correct query

### Metrics Always Zero

**Problem**: Reports show 0.00ms for all/some metrics

**Solutions**:
1. Check which timestamps are missing in profiling metadata
2. Verify system clock is synchronized (NTP)
3. Identify which component isn't setting timestamps

### Volatile Percentiles

**Problem**: Percentiles fluctuate too much

**Solution**: Increase window_size (2000, 5000, or larger)

### Stale Percentiles

**Problem**: Percentiles don't reflect recent changes

**Solution**: Decrease window_size (500, 1000)

## Related Documentation

- [Overall Profiling Guide](../../../docs/PROFILING.md) - Comprehensive profiling system documentation
- [Example Code](../../../examples/profiling_with_profiler_reaction.rs) - Working profiler example
- [Source Code](mod.rs) - Implementation details
