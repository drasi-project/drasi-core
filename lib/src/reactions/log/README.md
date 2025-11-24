# Log Reaction

A debugging and development reaction that outputs query results to the application's logging infrastructure.

## Purpose

The Log reaction is designed for:
- Development and debugging workflows
- Testing query results without external systems
- Monitoring data changes in real-time
- Capturing performance metrics with profiling

**Best for**: Development and testing environments. For production deployments with high throughput, use HTTP, gRPC, or SSE reactions instead.

## Configuration

### Configuration Settings

The Log Reaction supports the following configuration settings:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `id` | String | Unique identifier for the reaction | Any string | **(Required)** |
| `queries` | Array[String] | IDs of queries this reaction subscribes to | Array of query IDs | **(Required)** |
| `reaction_type` | String | Reaction type discriminator | "log" | **(Required)** |
| `auto_start` | Boolean | Whether to automatically start this reaction | true, false | `true` |
| `log_level` | LogLevel (enum) | Logging level for outputting query results. Determines which Rust log macro is used (trace!, debug!, info!, warn!, or error!) | `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"` (case-insensitive) | `"info"` |
| `priority_queue_capacity` | Integer (Optional) | Maximum events in priority queue before backpressure. Controls event queuing before the reaction processes them. Higher values allow more buffering but use more memory | Any positive integer | `10000` |

### Log Level Details

| Level | Use Case | Output Frequency |
|-------|----------|-----------------|
| `trace` | Extremely verbose debugging (library internals) | Very high |
| `debug` | Development debugging, includes profiling data | High |
| `info` | General monitoring, standard operational logging | Medium |
| `warn` | Alert conditions, important events | Low |
| `error` | Critical issues requiring attention | Very low |

**Note**: The log level must be enabled in your application's logging configuration (e.g., `RUST_LOG` environment variable) to see output. For example, setting `log_level: "debug"` in the reaction config requires `RUST_LOG=debug` or lower to actually display the logs.

### Configuration Examples

```yaml
reactions:
  - id: "sensor-logger"
    queries: ["sensor-monitor"]
    reaction_type: "log"
    auto_start: true
    priority_queue_capacity: 5000
    log_level: "info"
```

## Output Format

All log messages follow this pattern:
```
[REACTION_ID] Message content
```

### ADD Operation
```
[sensor-logger] Query 'sensor-monitor' results (1 items):
[sensor-logger] [ADD] id: sensor_01, temperature: 25.5, humidity: 60
```

### UPDATE Operation
```
[user-logger] Query 'user-activity' results (1 items):
[user-logger] [UPDATE] id: user123, status: active -> id: user123, status: inactive
```

### REMOVE Operation
```
[sensor-logger] Query 'cleanup-monitor' results (1 items):
[sensor-logger] [REMOVE] id: old_sensor_99, name: Deprecated Sensor
```

### Profiling Data (Debug Level)
When profiling is enabled, end-to-end latency is logged at debug level:
```
[sensor-logger] Query 'sensor-monitor' results (5 items):
[sensor-logger] [ADD] id: sensor_01, temperature: 25.5
...
[sensor-logger] End-to-end latency: 12.45ms
```

## Profiling Integration

The Log reaction captures performance metrics when profiling is enabled:

**Metrics Logged**:
- End-to-end latency (from source send to reaction complete)
- Always logged at `debug` level regardless of reaction's configured log level

**Enable Profiling**:
```yaml
# In your server configuration
profiling:
  enabled: true
  sampling_rate: 1.0  # Profile 100% of events
```

Set `RUST_LOG=debug` to see profiling latency messages.

## Best Practices

### Log Level Selection

| Scenario | Recommended Level |
|----------|------------------|
| Development | `debug` or `trace` |
| Testing | `info` |
| Low-volume production monitoring | `info` |
| Critical alerts only | `warn` or `error` |

### Performance Considerations

**Throughput Limits**:
- Low: < 10 events/sec (all levels safe)
- Medium: 10-100 events/sec (use `info` or higher)
- High: > 100 events/sec (use alternative reactions)

**Memory Usage**:
- Results are cloned for async processing
- Channel buffer defaults to 1000 items
- Large result sets can consume significant memory

## Troubleshooting

### No Output Visible

Check the following:
1. Verify `RUST_LOG` environment variable is set:
   ```bash
   RUST_LOG=info cargo run
   ```
2. Confirm log level hierarchy (app level must be >= reaction level)
3. Verify reaction is running by checking logs for "Started - receiving results"

### Missing Profiling Data

1. Enable debug logging: `RUST_LOG=debug`
2. Verify profiling is enabled in configuration
3. Check that sources are creating profiling metadata

### Performance Degradation

1. Reduce log level to `warn` or `error`
2. Enable profiling sampling to reduce overhead
3. Use dedicated monitoring reactions for high throughput scenarios

## Limitations

1. **Not for High Throughput**: Best for < 100 events/sec
2. **No Delivery Guarantees**: Logs can be lost on system crashes
3. **No Built-in Queryability**: Requires external log aggregation tools
4. **String Formatting Cost**: CPU overhead for every result
5. **Memory Growth**: Large result sets can consume memory
6. **No Aggregation**: Each result logged individually

For production deployments requiring high throughput, durability, or advanced monitoring, use HTTP/gRPC reactions with dedicated monitoring systems.
