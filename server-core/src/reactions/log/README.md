# Log Reaction

## Purpose

The Log reaction is a built-in debugging and monitoring reaction that outputs query results to the application's logging infrastructure. It formats and logs data changes (adds, updates, deletes) from continuous queries at configurable log levels.

### Use Cases

- **Development and Debugging**: Monitor query results in real-time during development to verify query behavior and data flow
- **System Monitoring**: Track specific data changes or events that pass through the Drasi pipeline
- **Performance Analysis**: Capture end-to-end latency metrics and profiling data for performance debugging
- **Troubleshooting**: Diagnose issues in the data processing pipeline by examining detailed change logs
- **Testing**: Validate query results and data transformations without requiring external systems

### When to Use This Reaction

Use the Log reaction when you need to:
- Debug continuous queries during development
- Monitor low-to-moderate throughput data streams
- Analyze performance characteristics with profiling data
- Verify data transformations before deploying production reactions
- Capture diagnostic information for troubleshooting

**Note**: The Log reaction is best suited for development, testing, and debugging scenarios. For high-throughput production environments, consider using reactions optimized for performance (e.g., HTTP, gRPC, or custom reactions).

## Configuration Properties

### log_level

- **Data Type**: String
- **Default Value**: `"info"`
- **Required**: No (optional)
- **Description**: Controls the logging level at which query results are emitted

**Supported Log Levels** (ordered from most to least verbose):

| Level | Description | Use Case |
|-------|-------------|----------|
| `trace` | Most detailed logging | Extremely verbose debugging, typically for library internals |
| `debug` | Detailed diagnostic information | Development debugging, performance analysis |
| `info` | Informational messages | General monitoring, standard operational logging |
| `warn` | Warning messages | Alert conditions, important events |
| `error` | Error messages | Critical issues requiring attention |

**Behavior**:
- All query results are logged at the configured level
- Invalid log level values default to `"info"`
- The log level must be enabled in the application's logging configuration to see output (e.g., via `RUST_LOG` environment variable)

## Configuration Examples

### YAML Configuration

#### Basic Configuration (Default Info Level)

```yaml
reactions:
  - id: "basic-logger"
    reaction_type: "log"
    queries: ["sensor-monitor"]
    auto_start: true
    properties:
      log_level: "info"
```

#### Debug Level for Development

```yaml
reactions:
  - id: "debug-logger"
    reaction_type: "log"
    queries: ["user-activity"]
    auto_start: true
    properties:
      log_level: "debug"
```

#### Warning Level for Critical Events

```yaml
reactions:
  - id: "alert-logger"
    reaction_type: "log"
    queries: ["high-temp-alert"]
    auto_start: true
    properties:
      log_level: "warn"
```

#### Multiple Queries with Trace Level

```yaml
reactions:
  - id: "multi-query-logger"
    reaction_type: "log"
    queries:
      - "query-one"
      - "query-two"
      - "query-three"
    auto_start: true
    properties:
      log_level: "trace"
```

#### Minimal Configuration (Uses Defaults)

```yaml
reactions:
  - id: "simple-logger"
    reaction_type: "log"
    queries: ["my-query"]
    properties: {}
```

### JSON Configuration

#### Basic Configuration

```json
{
  "reactions": [
    {
      "id": "basic-logger",
      "reaction_type": "log",
      "queries": ["sensor-monitor"],
      "auto_start": true,
      "properties": {
        "log_level": "info"
      }
    }
  ]
}
```

#### Debug Level Configuration

```json
{
  "reactions": [
    {
      "id": "debug-logger",
      "reaction_type": "log",
      "queries": ["user-activity"],
      "auto_start": true,
      "properties": {
        "log_level": "debug"
      }
    }
  ]
}
```

#### Multiple Reactions Configuration

```json
{
  "reactions": [
    {
      "id": "info-logger",
      "reaction_type": "log",
      "queries": ["general-monitor"],
      "auto_start": true,
      "properties": {
        "log_level": "info"
      }
    },
    {
      "id": "error-logger",
      "reaction_type": "log",
      "queries": ["error-tracking"],
      "auto_start": true,
      "properties": {
        "log_level": "error"
      }
    }
  ]
}
```

## Programmatic Construction in Rust

### Creating the Reaction Configuration

```rust
use drasi_server_core::config::ReactionConfig;
use std::collections::HashMap;
use serde_json::json;

// Create properties with log level
let mut properties = HashMap::new();
properties.insert(
    "log_level".to_string(),
    json!("debug")
);

// Create reaction configuration
let reaction_config = ReactionConfig {
    id: "my-log-reaction".to_string(),
    reaction_type: "log".to_string(),
    queries: vec!["query-id".to_string()],
    auto_start: true,
    properties,
};
```

### Starting the Reaction

```rust
use drasi_server_core::reactions::log::LogReaction;
use drasi_server_core::reactions::Reaction;
use drasi_server_core::channels::EventChannels;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create event channels
    let (channels, _receivers) = EventChannels::new();

    // Create the log reaction
    let log_reaction = LogReaction::new(
        reaction_config,
        channels.component_event_tx.clone()
    );

    // Create a receiver for query results
    let (_result_tx, result_rx) = mpsc::channel(100);

    // Start the reaction (spawns async task)
    log_reaction.start(result_rx).await?;

    // The reaction is now running and will log results as they arrive

    Ok(())
}
```

### Complete Integration Example

```rust
use drasi_server_core::{
    DrasiServerCore,
    DrasiServerCoreConfig,
    config::ReactionConfig
};
use std::collections::HashMap;
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("debug")
    ).init();

    // Load configuration from file
    let mut config = DrasiServerCoreConfig::load_from_file("config.yaml")?;

    // Or add log reaction programmatically
    let mut properties = HashMap::new();
    properties.insert("log_level".to_string(), json!("info"));

    config.reactions.push(ReactionConfig {
        id: "programmatic-logger".to_string(),
        reaction_type: "log".to_string(),
        queries: vec!["my-query".to_string()],
        auto_start: true,
        properties,
    });

    // Create and start DrasiServerCore
    let mut core = DrasiServerCore::new(std::sync::Arc::new(config.into()));
    core.initialize().await?;
    core.start().await?;

    // Log reaction is now active and logging query results

    Ok(())
}
```

### Setting Different Log Levels

```rust
use std::collections::HashMap;
use serde_json::json;

// Helper function to create properties with specific log level
fn create_log_properties(level: &str) -> HashMap<String, serde_json::Value> {
    let mut properties = HashMap::new();
    properties.insert("log_level".to_string(), json!(level));
    properties
}

// Create different configurations
let trace_props = create_log_properties("trace");
let debug_props = create_log_properties("debug");
let info_props = create_log_properties("info");
let warn_props = create_log_properties("warn");
let error_props = create_log_properties("error");
```

## Input Data Format

The Log reaction receives `QueryResult` structures from the query engine via Tokio channels.

### QueryResult Structure

```rust
pub struct QueryResult {
    /// ID of the query that produced this result
    pub query_id: String,

    /// Timestamp when the result was created
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Vector of result objects (each representing a data change)
    pub results: Vec<serde_json::Value>,

    /// Additional metadata about the query execution
    pub metadata: HashMap<String, serde_json::Value>,

    /// Optional profiling metadata for performance tracking
    pub profiling: Option<ProfilingMetadata>,
}
```

### Result Object Structure

Each element in the `results` vector has the following structure:

#### ADD Operation

```json
{
  "type": "add",
  "data": {
    "id": "user123",
    "name": "John Doe",
    "email": "john@example.com"
  }
}
```

#### UPDATE Operation

```json
{
  "type": "update",
  "before": {
    "id": "user123",
    "name": "John Doe",
    "status": "active"
  },
  "after": {
    "id": "user123",
    "name": "John Doe",
    "status": "inactive"
  }
}
```

#### DELETE/REMOVE Operation

```json
{
  "type": "remove",
  "data": {
    "id": "user123",
    "name": "John Doe"
  }
}
```

## Output Data Format

The Log reaction formats query results as structured log messages. Each result is formatted based on its operation type.

### Log Message Format

All log messages follow this pattern:

```
[REACTION_ID] Message content
```

Where:
- `REACTION_ID` is the configured ID of the log reaction
- Message content varies by operation type and includes formatted result data

### ADD Operation Format

```
[sensor-logger] Query 'sensor-monitor' results (1 items):
[sensor-logger] [ADD] id: sensor_01, temperature: 25.5, humidity: 60, timestamp: 2025-01-15T10:30:00Z
```

**Format Pattern**: `[ADD] field1: value1, field2: value2, ...`

### UPDATE Operation Format

```
[user-logger] Query 'user-activity' results (1 items):
[user-logger] [UPDATE] id: user123, name: John Doe, status: active -> id: user123, name: John Doe, status: inactive
```

**Format Pattern**: `[UPDATE] before_field1: before_value1, ... -> after_field1: after_value1, ...`

### DELETE/REMOVE Operation Format

```
[sensor-logger] Query 'cleanup-monitor' results (1 items):
[sensor-logger] [REMOVE] id: old_sensor_99, name: Deprecated Sensor, status: offline
```

**Format Pattern**: `[REMOVE] field1: value1, field2: value2, ...`

### Multiple Results Example

When a query produces multiple changes in a single result set:

```
[multi-logger] Query 'batch-monitor' results (3 items):
[multi-logger] [ADD] id: item1, value: 100
[multi-logger] [UPDATE] id: item2, value: 50 -> id: item2, value: 75
[multi-logger] [REMOVE] id: item3, value: 25
```

### Profiling Metadata in Logs

When profiling is enabled, the Log reaction automatically logs end-to-end latency after processing each result set:

```
[sensor-logger] Query 'sensor-monitor' results (5 items):
[sensor-logger] [ADD] id: sensor_01, temperature: 25.5
[sensor-logger] [ADD] id: sensor_02, temperature: 26.1
[sensor-logger] [ADD] id: sensor_03, temperature: 24.8
[sensor-logger] [ADD] id: sensor_04, temperature: 25.9
[sensor-logger] [ADD] id: sensor_05, temperature: 25.3
[sensor-logger] End-to-end latency: 12.45ms
```

### Startup and Lifecycle Messages

The Log reaction also emits lifecycle messages:

```
[sensor-logger] Started - receiving results from queries: ["sensor-monitor"]
[sensor-logger] Result channel closed, stopping reaction
```

### Empty Result Sets

Empty result sets (with no changes) are logged at debug level:

```
[sensor-logger] Received empty result set from query
```

## Profiling Integration

The Log reaction integrates with DrasiServerCore's profiling infrastructure to capture and log performance metrics.

### How Profiling Metadata is Captured

The profiling system tracks timestamps at each stage of the pipeline:

1. **Source Stage**: `source_receive_ns`, `source_send_ns`
2. **Query Stage**: `query_receive_ns`, `query_core_call_ns`, `query_core_return_ns`, `query_send_ns`
3. **Reaction Stage**: `reaction_receive_ns`, `reaction_complete_ns`

The Log reaction automatically captures two key timestamps:

```rust
// When the reaction receives the result
profiling.reaction_receive_ns = Some(timestamp_ns());

// After processing completes
profiling.reaction_complete_ns = Some(timestamp_ns());
```

### What Metrics are Logged

When profiling metadata is available, the Log reaction logs:

**End-to-End Latency**: Total time from source sending the event to reaction completing processing

```
[my-logger] End-to-end latency: 12.45ms
```

This latency is calculated as:
```rust
let total_latency_ns = reaction_complete_ns - source_send_ns;
```

The latency is logged at `debug` level regardless of the reaction's configured log level.

### End-to-End Latency Calculation

The end-to-end latency measurement captures the complete journey of a data change:

```
Source → Query → Reaction
   |       |         |
   |-------|---------|
     Total Latency
```

**Calculation**:
- **Start**: `source_send_ns` - when the source sends the change event
- **End**: `reaction_complete_ns` - when the reaction finishes processing
- **Result**: Latency in milliseconds with 2 decimal precision

**Example Log Output**:
```
[profiled-logger] End-to-end latency: 8.73ms
```

### Enabling/Disabling Profiling Data in Logs

Profiling in the Log reaction is **automatic** when profiling metadata is present in the `QueryResult`. No configuration is required in the reaction itself.

To control profiling globally:

**Via Configuration File**:
```yaml
server_core:
  profiling:
    enabled: true
    sampling_rate: 1.0  # Profile 100% of events (default)
    include_bootstrap: true
```

**Via Environment Variables** (when supported by your application):
```bash
RUST_LOG=debug cargo run
```

The `debug` log level is required to see profiling latency messages, as they are always logged at `debug` level.

**Programmatically**:
```rust
use drasi_server_core::profiling::ProfilingConfig;

let profiling_config = ProfilingConfig::enabled()
    .with_sampling_rate(0.1)  // Profile 10% of events
    .with_include_bootstrap(false);
```

### Profiling Data Structure

The complete profiling metadata structure:

```rust
pub struct ProfilingMetadata {
    pub source_ns: Option<u64>,              // External source timestamp
    pub reactivator_start_ns: Option<u64>,   // Reactivator start
    pub reactivator_end_ns: Option<u64>,     // Reactivator end
    pub source_receive_ns: Option<u64>,      // Source receives event
    pub source_send_ns: Option<u64>,         // Source sends to channel
    pub query_receive_ns: Option<u64>,       // Query receives from channel
    pub query_core_call_ns: Option<u64>,     // Before drasi-core processing
    pub query_core_return_ns: Option<u64>,   // After drasi-core processing
    pub query_send_ns: Option<u64>,          // Query sends result
    pub reaction_receive_ns: Option<u64>,    // Reaction receives result
    pub reaction_complete_ns: Option<u64>,   // Reaction completes processing
}
```

## Log Analysis

### Filtering Logs by Query ID

The Log reaction includes the query ID in each result log entry, making it easy to filter logs:

**Using grep**:
```bash
# Filter logs for specific query
grep "Query 'sensor-monitor'" app.log

# Filter logs for specific reaction
grep "\[sensor-logger\]" app.log

# Combine query and reaction filters
grep "\[sensor-logger\]" app.log | grep "Query 'sensor-monitor'"
```

**Using RUST_LOG environment variable**:
```bash
# Set log level for specific reaction (if supported by your app)
RUST_LOG=drasi_server_core::reactions::log=debug cargo run
```

### Searching for Specific Operations

**Find all ADD operations**:
```bash
grep "\[ADD\]" app.log
```

**Find all UPDATE operations**:
```bash
grep "\[UPDATE\]" app.log
```

**Find all REMOVE/DELETE operations**:
```bash
grep "\[REMOVE\]" app.log
```

**Find operations for specific entity**:
```bash
# Find all operations on user123
grep "id: user123" app.log
```

### Analyzing Performance from Log Output

**Extract latency measurements**:
```bash
# Get all latency measurements
grep "End-to-end latency" app.log

# Calculate average latency (requires awk)
grep "End-to-end latency" app.log | \
  awk -F': ' '{sum += $2; count++} END {print "Average:", sum/count, "ms"}'
```

**Find slow operations** (> 100ms):
```bash
grep "End-to-end latency" app.log | \
  awk -F': ' '{latency = $2 + 0; if (latency > 100) print}'
```

**Identify performance patterns**:
```bash
# Count operations by type
grep -o "\[ADD\]\|\[UPDATE\]\|\[REMOVE\]" app.log | sort | uniq -c

# Results processed per query
grep "results (" app.log | \
  awk -F'[()]' '{print $2}' | \
  awk '{sum += $1; count++} END {print "Avg results per batch:", sum/count}'
```

### Using Log Aggregation Tools

**Structured Logging Best Practices**:

For production use with log aggregation tools (e.g., ELK, Splunk, Datadog), consider:

1. **JSON Formatting**: Parse log messages into structured JSON
2. **Add Correlation IDs**: Track events across the pipeline
3. **Index Key Fields**: Query ID, reaction ID, operation type, timestamps
4. **Set Up Dashboards**: Visualize throughput, latency, error rates

**Example ELK Query** (conceptual):
```
message: "[sensor-logger]" AND "End-to-end latency"
```

**Example Splunk Query** (conceptual):
```
source="app.log" "[sensor-logger]" | stats avg(latency_ms) by query_id
```

### Pattern Matching for Debugging

**Identify errors or missing data**:
```bash
# Look for empty result sets
grep "Received empty result set" app.log

# Find reaction lifecycle events
grep "Started - receiving results" app.log
grep "Result channel closed" app.log
```

**Track data flow**:
```bash
# Follow a specific entity through operations
grep "id: sensor_01" app.log

# Timeline of operations
grep -E "\[ADD\]|\[UPDATE\]|\[REMOVE\]" app.log | \
  grep "id: sensor_01" | \
  awk '{print $1, $2, $3}'
```

## Troubleshooting

### Log Level Not Showing Output

**Problem**: You've configured a log level, but don't see any output.

**Solutions**:

1. **Check Application Log Configuration**:
   ```bash
   # Set RUST_LOG to enable logging at your desired level
   RUST_LOG=info cargo run

   # Enable debug level for more verbosity
   RUST_LOG=debug cargo run

   # Enable trace for maximum verbosity
   RUST_LOG=trace cargo run
   ```

2. **Verify Log Level Hierarchy**: Ensure your application's log level is at least as verbose as the reaction's log level:
   - If reaction uses `debug` but `RUST_LOG=info`, debug messages won't appear
   - Solution: Set `RUST_LOG=debug` or lower

3. **Check Reaction Configuration**:
   ```yaml
   properties:
     log_level: "debug"  # Ensure this matches your intent
   ```

4. **Verify Reaction is Running**:
   ```bash
   # Look for startup message
   grep "Started - receiving results" app.log
   ```

### Missing Profiling Data

**Problem**: End-to-end latency logs are not appearing.

**Solutions**:

1. **Enable Debug Logging**: Profiling latency is logged at debug level:
   ```bash
   RUST_LOG=debug cargo run
   ```

2. **Verify Profiling is Enabled Globally**:
   ```yaml
   server_core:
     profiling:
       enabled: true
   ```

3. **Check for Profiling Metadata**: Ensure sources are creating profiling metadata:
   ```rust
   let profiling = ProfilingMetadata::new();
   ```

4. **Verify Results Have Profiling Data**: Check that QueryResult includes profiling:
   ```rust
   QueryResult::with_profiling(query_id, timestamp, results, metadata, profiling)
   ```

### Log Verbosity Performance Impact

**Problem**: Logging is causing performance degradation.

**Solutions**:

1. **Reduce Log Level**: Use less verbose levels for production:
   ```yaml
   properties:
     log_level: "warn"  # or "error" for critical issues only
   ```

2. **Enable Sampling**: Use profiling sampling to reduce overhead:
   ```yaml
   server_core:
     profiling:
       enabled: true
       sampling_rate: 0.1  # Profile only 10% of events
   ```

3. **Async Logging**: Ensure your logging infrastructure is asynchronous

4. **Buffer Configuration**: Adjust Tokio channel buffer sizes if needed:
   ```rust
   // In your application's channel setup
   let (tx, rx) = mpsc::channel(10000);  // Increase buffer
   ```

5. **Consider Alternatives**: For high-throughput production, use dedicated monitoring reactions instead of Log reaction

### Log Rotation Considerations

**Problem**: Log files growing too large.

**Solutions**:

1. **Enable Log Rotation**: Configure your logging backend (env_logger, tracing, etc.) to rotate logs:
   ```rust
   // Example with env_logger
   env_logger::Builder::from_env(
       env_logger::Env::default().default_filter_or("info")
   )
   .target(env_logger::Target::Stdout)  // Use stdout for container logging
   .init();
   ```

2. **Use External Log Rotation**: Configure logrotate (Linux) or equivalent:
   ```
   /var/log/myapp/*.log {
       daily
       rotate 7
       compress
       delaycompress
       missingok
       notifempty
   }
   ```

3. **Log to stdout/stderr**: In containerized environments, let container orchestration handle logs:
   ```yaml
   # Let Docker/Kubernetes handle log management
   # Logs automatically go to stdout
   ```

4. **Filter by Severity**: Only log important events in production:
   ```yaml
   properties:
     log_level: "warn"  # Reduces volume significantly
   ```

### Empty Result Set Messages

**Problem**: Seeing many "Received empty result set" messages.

**Solutions**:

1. **This is Normal Behavior**: Empty result sets occur when:
   - Query doesn't match the incoming data
   - Data changes don't affect the query result
   - Filters exclude all changes

2. **Review Query Logic**: If unexpected, check your query:
   ```cypher
   MATCH (n:Sensor)
   WHERE n.temperature > 30  -- Verify threshold is appropriate
   RETURN n
   ```

3. **Reduce Noise**: Empty results are logged at `debug` level, so use `info` or higher in production:
   ```yaml
   properties:
     log_level: "info"  # Won't show empty result messages
   ```

## Limitations

### Performance Impact of Different Log Levels

The Log reaction has different performance characteristics based on the configured log level:

| Log Level | Relative Overhead | When to Use |
|-----------|-------------------|-------------|
| `trace` | Highest | Development only, extremely detailed debugging |
| `debug` | High | Development, performance analysis with profiling |
| `info` | Moderate | Testing, low-to-moderate throughput monitoring |
| `warn` | Low | Production, critical event monitoring |
| `error` | Minimal | Production, error-only monitoring |

**Impact Factors**:
- String formatting is performed for every result
- I/O operations to write logs can block
- Higher verbosity = more data = slower processing

### Log Volume Considerations

**Estimated Log Volume** (approximate):

| Scenario | Events/sec | Log Size/Event | Volume/Hour |
|----------|------------|----------------|-------------|
| Low throughput | 10 | 200 bytes | ~7 MB |
| Medium throughput | 100 | 200 bytes | ~70 MB |
| High throughput | 1,000 | 200 bytes | ~700 MB |
| Very high throughput | 10,000 | 200 bytes | ~7 GB |

**Considerations**:
- Each result entry generates at least 2 log lines (header + data)
- UPDATE operations generate longer messages (before + after)
- Profiling adds additional log lines
- Multiple results per QueryResult multiply the volume

### String Formatting Overhead

The Log reaction performs string formatting for every result:

```rust
// Each result requires:
// 1. Type extraction
// 2. Field iteration
// 3. Value formatting
// 4. String concatenation
let formatted = format!("[{}] {}: {}, ...", result_type, key1, val1);
```

**Overhead Sources**:
- JSON object iteration and conversion
- String allocation for each field
- Format string processing
- Clone operations for async logging

**Mitigation**:
- Use higher log levels (warn/error) to reduce formatting frequency
- Consider dedicated monitoring reactions for high-throughput scenarios

### Memory Usage for Large Results

**Memory Considerations**:

1. **Result Cloning**: Results are processed in an async task, requiring data to be cloned
2. **String Buffers**: Formatted strings are allocated in memory
3. **Channel Buffering**: Results queue in Tokio channels (default: 1000 items)

**Large Result Impact**:
- QueryResult with 1000 items × 1KB each = ~1MB per result
- Channel buffer of 1000 × 1MB = up to 1GB memory usage (worst case)

**Recommendations**:
- Monitor memory usage in production
- Use batching in queries to limit result size
- Consider pagination for large result sets

### Not Suitable for High-Throughput Production Scenarios

The Log reaction is **not recommended** for high-throughput production deployments:

**Limitations**:
- **Synchronous I/O**: Logging can block processing
- **No Backpressure Control**: Can't throttle upstream data flow
- **Limited Scalability**: Single-threaded processing per reaction
- **No Delivery Guarantees**: Logs can be lost if system crashes
- **No Structured Retention**: No built-in archival or querying

**Production Alternatives**:

| Scenario | Recommended Reaction |
|----------|----------------------|
| High throughput | HTTP, gRPC, or Platform reactions |
| Data persistence | Custom database reaction |
| Real-time monitoring | SSE or WebSocket reactions |
| Analytics | Stream to data warehouse |
| Alerting | Custom alerting reaction |

**When Log Reaction is Appropriate in Production**:
- Low-volume critical alerts (< 10 events/sec)
- Debugging specific issues temporarily
- Monitoring during deployment validation
- Compliance logging for audit trails (with proper log management)

### Summary of Key Limitations

1. **Throughput**: Not designed for > 100 events/second sustained load
2. **Durability**: Logs may be lost without proper log management
3. **Queryability**: No built-in search or analytics capabilities
4. **Formatting Cost**: CPU overhead for string formatting on every result
5. **Memory**: Unbounded growth possible with very large result sets
6. **No Aggregation**: Each result logged individually, no summarization

For production deployments requiring high throughput, durability, or advanced monitoring capabilities, consider implementing custom reactions or using the HTTP/gRPC reactions with dedicated monitoring systems.
