# Log Reaction

A development and debugging reaction that outputs query results to the console for monitoring data changes in real-time.

## Overview

The Log Reaction provides console logging of continuous query results, making it ideal for development, debugging, and low-volume monitoring scenarios. It subscribes to one or more queries and prints formatted output to stdout showing how data changes over time.

### Key Capabilities

- **Real-time Monitoring**: Displays query results as they arrive with timestamp ordering
- **Custom Formatting**: Supports Handlebars templates for customized output
- **Operation Tracking**: Shows ADD, UPDATE, and DELETE operations with before/after states
- **Profiling Support**: Captures and logs end-to-end latency metrics when enabled
- **Ordered Processing**: Uses priority queue to process results in timestamp order

### Use Cases

- **Development**: Monitor query behavior during development
- **Debugging**: Inspect query results and data transformations
- **Testing**: Verify query correctness without external dependencies
- **Performance Analysis**: Measure end-to-end latency with profiling
- **Demo/Prototyping**: Quick visualization of data changes

**Best for**: Development and testing environments with low to medium throughput (< 100 events/sec).

**Not recommended for**: High-throughput production deployments. Use HTTP, gRPC, or SSE reactions for production monitoring.

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides a fluent, type-safe API for creating LogReaction instances:

```rust
use drasi_reaction_log::LogReaction;

let reaction = LogReaction::builder("my-logger")
    .from_query("sensor-monitor")
    .from_query("user-activity")
    .with_added_template("[NEW] Sensor {{after.id}}: temp={{after.temperature}}°C")
    .with_updated_template("[CHG] {{after.id}}: {{before.temperature}}°C -> {{after.temperature}}°C")
    .with_deleted_template("[DEL] Sensor {{before.id}} removed")
    .with_priority_queue_capacity(5000)
    .with_auto_start(true)
    .build();

// Add to DrasiLib (event channel is automatically injected)
drasi.add_reaction(Arc::new(reaction)).await?;
```

### Config Struct Approach

For programmatic configuration or deserialization scenarios:

```rust
use drasi_reaction_log::{LogReaction, LogReactionConfig};

let config = LogReactionConfig {
    added_template: Some("[ADD] {{after.name}}".to_string()),
    updated_template: Some("[UPD] {{before.value}} -> {{after.value}}".to_string()),
    deleted_template: Some("[DEL] {{before.name}}".to_string()),
};

let reaction = LogReaction::new(
    "my-logger",
    vec!["query1".to_string(), "query2".to_string()],
    config
);

drasi.add_reaction(Arc::new(reaction)).await?;
```

## Configuration Options

### Core Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `id` | Unique identifier for the reaction | `String` | Any valid string | **(Required)** |
| `queries` | Query IDs to subscribe to | `Vec<String>` | Array of query IDs | **(Required)** |
| `auto_start` | Whether to start automatically when added | `bool` | `true`, `false` | `true` |
| `priority_queue_capacity` | Maximum events in priority queue | `usize` | Any positive integer | `10000` |

### Template Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `added_template` | Handlebars template for ADD events | `Option<String>` | Valid Handlebars template | `None` (JSON output) |
| `updated_template` | Handlebars template for UPDATE events | `Option<String>` | Valid Handlebars template | `None` (JSON output) |
| `deleted_template` | Handlebars template for DELETE events | `Option<String>` | Valid Handlebars template | `None` (JSON output) |

### Template Variables

Templates have access to the following context variables:

**ADD Events:**
- `after` - The new data being added
- `query_name` - Name of the query producing this result
- `operation` - Operation type (always "ADD")

**UPDATE Events:**
- `before` - Data before the change
- `after` - Data after the change
- `data` - Raw data field from the result
- `query_name` - Name of the query producing this result
- `operation` - Operation type (always "UPDATE")

**DELETE Events:**
- `before` - Data being removed
- `query_name` - Name of the query producing this result
- `operation` - Operation type (always "DELETE")

### Template Helpers

The `json` helper is available for converting values to JSON:

```handlebars
Full object: {{json after}}
```

## Output Schema

All log output follows this format pattern:

```
[REACTION_ID] Header message
[REACTION_ID]   Event details
```

### Default Output (No Templates)

**ADD Operation:**
```
[sensor-logger] Query 'sensor-monitor' (1 items):
[sensor-logger]   [ADD] {"id":"sensor_01","temperature":25.5,"humidity":60}
```

**UPDATE Operation:**
```
[sensor-logger] Query 'sensor-monitor' (1 items):
[sensor-logger]   [UPDATE] {"id":"sensor_01","temperature":25.5} -> {"id":"sensor_01","temperature":26.3}
```

**DELETE Operation:**
```
[sensor-logger] Query 'sensor-monitor' (1 items):
[sensor-logger]   [DELETE] {"id":"sensor_99","temperature":22.1}
```

### Custom Template Output

With templates configured:

```rust
let reaction = LogReaction::builder("sensor-logger")
    .from_query("sensor-monitor")
    .with_added_template("[NEW] Sensor {{after.id}}: {{after.temperature}}°C")
    .with_updated_template("[CHG] {{after.id}}: {{before.temperature}}°C -> {{after.temperature}}°C")
    .with_deleted_template("[DEL] Sensor {{before.id}}")
    .build();
```

Output:
```
[sensor-logger] Query 'sensor-monitor' (1 items):
[sensor-logger]   [NEW] Sensor sensor_01: 25.5°C
[sensor-logger]   [CHG] sensor_01: 25.5°C -> 26.3°C
[sensor-logger]   [DEL] Sensor sensor_99
```

### Profiling Output

When profiling is enabled (`RUST_LOG=debug`):

```
[sensor-logger] Query 'sensor-monitor' (3 items):
[sensor-logger]   [ADD] {"id":"sensor_01","temperature":25.5}
[sensor-logger]   [ADD] {"id":"sensor_02","temperature":23.2}
[sensor-logger]   [ADD] {"id":"sensor_03","temperature":27.8}
[sensor-logger] End-to-end latency: 12.45ms
```

## Usage Examples

### Basic Logging

Simple logging with default JSON output:

```rust
use drasi_reaction_log::LogReaction;
use std::sync::Arc;

let reaction = LogReaction::builder("basic-logger")
    .from_query("my-query")
    .build();

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Multi-Query Monitoring

Subscribe to multiple queries:

```rust
let reaction = LogReaction::builder("multi-logger")
    .from_query("sensor-data")
    .from_query("user-activity")
    .from_query("system-alerts")
    .build();

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Custom Formatting

Use templates for readable output:

```rust
let reaction = LogReaction::builder("formatted-logger")
    .from_query("inventory")
    .with_added_template("✓ Added: {{after.product_name}} ({{after.quantity}} units)")
    .with_updated_template("↻ Updated: {{after.product_name}} stock: {{before.quantity}} → {{after.quantity}}")
    .with_deleted_template("✗ Removed: {{before.product_name}}")
    .build();

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Performance Tuning

Adjust queue capacity for high-volume scenarios:

```rust
let reaction = LogReaction::builder("high-volume-logger")
    .from_query("events")
    .with_priority_queue_capacity(50000)  // Increased buffer
    .build();

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Conditional Auto-Start

Create but don't start immediately:

```rust
let reaction = LogReaction::builder("manual-logger")
    .from_query("debug-query")
    .with_auto_start(false)  // Don't start automatically
    .build();

drasi.add_reaction(Arc::new(reaction)).await?;

// Start manually when needed
drasi.start_reaction("manual-logger").await?;
```

### Complex Template with JSON Helper

```rust
let reaction = LogReaction::builder("complex-logger")
    .from_query("user-events")
    .with_added_template(r#"
New User: {{after.name}} ({{after.email}})
  Full data: {{json after}}
"#)
    .with_updated_template(r#"
User {{after.id}} changed:
  Before: {{json before}}
  After:  {{json after}}
"#)
    .build();

drasi.add_reaction(Arc::new(reaction)).await?;
```

## Performance Considerations

### Throughput Limits

| Scenario | Events/Sec | Recommendation |
|----------|-----------|----------------|
| Low Volume | < 10 | Safe for all configurations |
| Medium Volume | 10-100 | Monitor CPU usage, consider templates |
| High Volume | > 100 | Use HTTP/gRPC/SSE reactions instead |

### Memory Usage

- **Priority Queue**: Buffers up to `priority_queue_capacity` events (default 10,000)
- **Result Cloning**: Each result is cloned for async processing
- **Template Rendering**: Minimal overhead for simple templates
- **Large Result Sets**: Consider pagination or filtering at query level

### CPU Impact

- **String Formatting**: Every result requires string formatting and console I/O
- **JSON Serialization**: Default output serializes full objects to JSON
- **Template Rendering**: Handlebars templates add minimal overhead
- **Console I/O**: Blocking writes to stdout can impact throughput

## Profiling Integration

The LogReaction automatically captures performance metrics when profiling is enabled.

### Enable Profiling

Set the Rust log level to debug:

```bash
RUST_LOG=debug cargo run
```

### Metrics Captured

- **reaction_receive_ns**: Timestamp when reaction receives the result
- **reaction_complete_ns**: Timestamp when reaction finishes processing
- **End-to-end Latency**: Time from source send to reaction complete

### Profiling Output

```
[sensor-logger] Query 'sensor-monitor' (5 items):
[sensor-logger]   [ADD] {"id":"sensor_01","temperature":25.5}
[sensor-logger]   [ADD] {"id":"sensor_02","temperature":23.2}
[sensor-logger]   [ADD] {"id":"sensor_03","temperature":27.8}
[sensor-logger]   [ADD] {"id":"sensor_04","temperature":24.1}
[sensor-logger]   [ADD] {"id":"sensor_05","temperature":26.9}
[sensor-logger] End-to-end latency: 8.32ms
```

## Troubleshooting

### No Output Visible

**Symptoms**: Reaction starts but no output appears

**Solutions**:
1. Check query is producing results
2. Verify reaction is subscribed to correct query IDs
3. Check reaction status: `drasi.get_reaction_status("my-logger").await`
4. Enable debug logging: `RUST_LOG=debug`

### Template Rendering Errors

**Symptoms**: JSON output instead of template output

**Solutions**:
1. Check template syntax (Handlebars format)
2. Verify variable names match available context
3. Look for error logs: `RUST_LOG=debug`
4. Test template with simple expressions first

### Performance Degradation

**Symptoms**: Slow processing, increasing latency

**Solutions**:
1. Reduce log output volume (filter at query level)
2. Increase `priority_queue_capacity` to buffer more events
3. Simplify templates (avoid complex logic)
4. Consider switching to HTTP/gRPC reactions for production

### Memory Growth

**Symptoms**: Increasing memory usage over time

**Solutions**:
1. Reduce `priority_queue_capacity` if set too high
2. Check for large result sets (consider query-level filtering)
3. Monitor queue depth under load
4. Ensure queries aren't producing unbounded result sets

## Limitations

1. **Throughput**: Not suitable for high-volume production (> 100 events/sec)
2. **Persistence**: No delivery guarantees; output can be lost on crashes
3. **Queryability**: Requires external log aggregation for analysis
4. **Blocking I/O**: Console writes can block the processing task
5. **No Buffering Control**: All results are processed immediately
6. **Single Output**: Only outputs to stdout (no file rotation)

For production deployments requiring high throughput, durability, or advanced monitoring, use dedicated reactions:
- **HTTP Reaction**: Webhook delivery to monitoring systems
- **gRPC Reaction**: Streaming to gRPC services
- **SSE Reaction**: Server-Sent Events for web clients

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
