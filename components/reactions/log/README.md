# Log Reaction

A development and debugging reaction that outputs query results to the console for monitoring data changes in real-time.

## Overview

The Log Reaction provides console logging of continuous query results, making it ideal for development, debugging, and low-volume monitoring scenarios. It subscribes to one or more queries and prints formatted output directly to stdout (console) showing how data changes over time.

**Output Method**: Uses `println!` to write directly to stdout without requiring logger initialization. All output appears immediately in the terminal where the application is running.

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

#### Default Template for All Queries

Set a default template that applies to all queries:

```rust
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};

// Define a default template that applies to all queries
let default_template = QueryConfig {
    added: Some(TemplateSpec {
        template: "[NEW] {{after.id}}".to_string(),
    }),
    updated: Some(TemplateSpec {
        template: "[CHG] {{after.id}}: {{before.value}} -> {{after.value}}".to_string(),
    }),
    deleted: Some(TemplateSpec {
        template: "[DEL] {{before.id}}".to_string(),
    }),
};

let reaction = LogReaction::builder("my-logger")
    .with_queries(vec![
        "sensor-monitor".to_string(),
        "user-activity".to_string(),
        "system-metrics".to_string(),
    ])
    .with_default_template(default_template)
    .build()?; // Returns Result - validates templates

// Add to DrasiLib (event channel is automatically injected)
drasi.add_reaction(Arc::new(reaction)).await?;
```

**Validation**: The `build()` method validates all templates and ensures routes match subscribed queries. Returns `Err` if:
- Any template has invalid Handlebars syntax
- A route query ID doesn't match any subscribed query

#### Per-Query Custom Templates

Override default template for specific queries:

```rust
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};

// Define custom templates for different queries
let sensor_config = QueryConfig {
    added: Some(TemplateSpec {
        template: "[SENSOR] New: {{after.sensor_id}} - {{after.temperature}}°C".to_string(),
    }),
    updated: Some(TemplateSpec {
        template: "[SENSOR] {{after.sensor_id}}: {{before.temperature}}°C -> {{after.temperature}}°C".to_string(),
    }),
    deleted: Some(TemplateSpec {
        template: "[SENSOR] Removed: {{before.sensor_id}}".to_string(),
    }),
};

let reaction = LogReaction::builder("sensor-logger")
    .with_query("sensor-readings")
    .with_route("sensor-readings", sensor_config)
    .build()?; // Validates templates and routes

drasi.add_reaction(Arc::new(reaction)).await?;
```

**Route Validation**: Routes must match subscribed queries (supports dotted notation: `source.query` matches route `query`).

**Template Priority:**
1. Query-specific routes (highest priority)
2. Default template (fallback)
3. Raw JSON output (when no template provided)

### Config Struct Approach

For programmatic configuration or deserialization scenarios:

#### Basic Configuration

```rust
use drasi_reaction_log::{LogReaction, LogReactionConfig, QueryConfig, TemplateSpec};

let default_template = QueryConfig {
    added: Some(TemplateSpec {
        template: "[ADD] {{after.name}}".to_string(),
    }),
    updated: Some(TemplateSpec {
        template: "[UPD] {{before.value}} -> {{after.value}}".to_string(),
    }),
    deleted: Some(TemplateSpec {
        template: "[DEL] {{before.name}}".to_string(),
    }),
};

let config = LogReactionConfig {
    routes: HashMap::new(),
    default_template: Some(default_template),
};

let reaction = LogReaction::new(
    "my-logger",
    vec!["query1".to_string(), "query2".to_string()],
    config
);

drasi.add_reaction(Arc::new(reaction)).await?;
```

#### With Per-Query Routes

```rust
use drasi_reaction_log::{LogReaction, LogReactionConfig, QueryConfig, TemplateSpec};
use std::collections::HashMap;

let mut routes = HashMap::new();
routes.insert("sensor-query".to_string(), QueryConfig {
    added: Some(TemplateSpec {
        template: "[SENSOR] {{after.id}}: {{after.temperature}}°C".to_string(),
    }),
    updated: None,  // Falls back to default
    deleted: None,  // Falls back to default
});

let default_template = QueryConfig {
    added: Some(TemplateSpec {
        template: "[DEFAULT] {{after.id}}".to_string(),
    }),
    updated: Some(TemplateSpec {
        template: "[DEFAULT-UPD] {{after.id}}".to_string(),
    }),
    deleted: Some(TemplateSpec {
        template: "[DEFAULT-DEL] {{before.id}}".to_string(),
    }),
};

let config = LogReactionConfig {
    routes,
    default_template: Some(default_template),
};

let reaction = LogReaction::new(
    "my-logger",
    vec!["sensor-query".to_string(), "other-query".to_string()],
    config
)?; // Returns Result - validates templates and routes

use std::sync::Arc;
drasi.add_reaction(Arc::new(reaction)).await?;
```

## Validation

Both `new()` constructor and `build()` builder method validate configuration at creation time:

**Template Validation**:
- All Handlebars templates are compiled to check syntax
- Invalid templates return `Err` with detailed error message
- Empty templates are valid (will use JSON output)

**Route Validation**:
- All route query IDs must match subscribed queries
- Supports exact match or dotted notation (`source.query` matches route `query`)
- Unmatched routes return `Err` listing subscribed queries

**Example**:
```rust
// ❌ Error: Invalid template syntax
let result = LogReaction::builder("test")
    .with_query("q1")
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec {
            template: "{{unclosed".to_string(), // Missing }}
        }),
        updated: None,
        deleted: None,
    })
    .build()?;
// Returns: Err("Invalid default template: Invalid template...")

// ❌ Error: Route doesn't match query
let result = LogReaction::builder("test")
    .with_query("sensor-data")
    .with_route("wrong-query", sensor_config)
    .build()?;
// Returns: Err("Route 'wrong-query' does not match any subscribed query...")
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

Templates can be configured at two levels:

1. **Default Template**: Applied to all queries unless overridden
2. **Per-Query Routes**: Override default for specific queries

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `default_template` | Default template configuration for all queries | `Option<QueryConfig>` | QueryConfig with templates | `None` (JSON output) |
| `routes` | Per-query template configurations | `HashMap<String, QueryConfig>` | Map of query ID to QueryConfig | `{}` (empty) |

**QueryConfig Structure:**
- `added`: Optional `TemplateSpec` for ADD operations
- `updated`: Optional `TemplateSpec` for UPDATE operations  
- `deleted`: Optional `TemplateSpec` for DELETE operations

**TemplateSpec Structure:**
- `template`: Handlebars template string for formatting

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
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};

let default_template = QueryConfig {
    added: Some(TemplateSpec {
        template: "[NEW] Sensor {{after.id}}: {{after.temperature}}°C".to_string(),
    }),
    updated: Some(TemplateSpec {
        template: "[CHG] {{after.id}}: {{before.temperature}}°C -> {{after.temperature}}°C".to_string(),
    }),
    deleted: Some(TemplateSpec {
        template: "[DEL] Sensor {{before.id}}".to_string(),
    }),
};

let reaction = LogReaction::builder("sensor-logger")
    .with_query("sensor-monitor")
    .with_default_template(default_template)
    .build()?;
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
    .with_query("my-query")
    .build()?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Multi-Query Monitoring

Subscribe to multiple queries:

```rust
let reaction = LogReaction::builder("multi-logger")
    .with_queries(vec![
        "sensor-data".to_string(),
        "user-activity".to_string(),
        "system-alerts".to_string(),
    ])
    .build()?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Custom Formatting

Use templates for readable output:

```rust
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};

let inventory_template = QueryConfig {
    added: Some(TemplateSpec {
        template: "✓ Added: {{after.product_name}} ({{after.quantity}} units)".to_string(),
    }),
    updated: Some(TemplateSpec {
        template: "↻ Updated: {{after.product_name}} stock: {{before.quantity}} → {{after.quantity}}".to_string(),
    }),
    deleted: Some(TemplateSpec {
        template: "✗ Removed: {{before.product_name}}".to_string(),
    }),
};

let reaction = LogReaction::builder("formatted-logger")
    .with_query("inventory")
    .with_default_template(inventory_template)
    .build()?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Performance Tuning

Adjust queue capacity for high-volume scenarios:

```rust
let reaction = LogReaction::builder("high-volume-logger")
    .with_query("events")
    .with_priority_queue_capacity(50000)  // Increased buffer
    .build()?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Conditional Auto-Start

Create but don't start immediately:

```rust
let reaction = LogReaction::builder("manual-logger")
    .with_query("debug-query")
    .with_auto_start(false)  // Don't start automatically
    .build()?;

drasi.add_reaction(Arc::new(reaction)).await?;

// Start manually when needed
drasi.start_reaction("manual-logger").await?;
```

### Complex Template with JSON Helper

```rust
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};

let user_template = QueryConfig {
    added: Some(TemplateSpec {
        template: r#"New User: {{after.name}} ({{after.email}})
  Full data: {{json after}}"#.to_string(),
    }),
    updated: Some(TemplateSpec {
        template: r#"User {{after.id}} changed:
  Before: {{json before}}
  After:  {{json after}}"#.to_string(),
    }),
    deleted: None,
};

let reaction = LogReaction::builder("complex-logger")
    .with_query("user-events")
    .with_default_template(user_template)
    .build()?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

### Per-Query Templates

Different formatting for different queries:

```rust
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};

// Default template for all queries
let default_template = QueryConfig {
    added: Some(TemplateSpec {
        template: "[DEFAULT] {{after.id}}".to_string(),
    }),
    updated: Some(TemplateSpec {
        template: "[DEFAULT] {{after.id}} updated".to_string(),
    }),
    deleted: None,
};

// Sensor-specific template
let sensor_config = QueryConfig {
    added: Some(TemplateSpec {
        template: "🌡️  Sensor {{after.id}}: {{after.temperature}}°C, {{after.humidity}}%".to_string(),
    }),
    updated: Some(TemplateSpec {
        template: "🌡️  Sensor {{after.id}}: {{before.temperature}}°C → {{after.temperature}}°C".to_string(),
    }),
    deleted: Some(TemplateSpec {
        template: "🌡️  Sensor {{before.id}} offline".to_string(),
    }),
};

// User activity template
let user_config = QueryConfig {
    added: Some(TemplateSpec {
        template: "👤 New login: {{after.username}} from {{after.ip_address}}".to_string(),
    }),
    updated: None,
    deleted: Some(TemplateSpec {
        template: "👤 Logout: {{before.username}}".to_string(),
    }),
};

// System alerts template
let alert_config = QueryConfig {
    added: Some(TemplateSpec {
        template: "⚠️  ALERT: {{after.severity}} - {{after.message}}".to_string(),
    }),
    updated: None,
    deleted: None,
};

let reaction = LogReaction::builder("multi-source-logger")
    .with_queries(vec![
        "sensor-data".to_string(),
        "user-activity".to_string(),
        "system-alerts".to_string(),
    ])
    .with_default_template(default_template)
    .with_route("sensor-data", sensor_config)
    .with_route("user-activity", user_config)
    .with_route("system-alerts", alert_config)
    .build()?;

drasi.add_reaction(Arc::new(reaction)).await?;
```

**Output:**
```
[multi-source-logger] Query 'sensor-data' (2 items):
[multi-source-logger]   🌡️  Sensor sensor_01: 25.5°C, 60%
[multi-source-logger]   🌡️  Sensor sensor_02: 22.3°C, 55%
[multi-source-logger] Query 'user-activity' (1 items):
[multi-source-logger]   👤 New login: john_doe from 192.168.1.10
[multi-source-logger] Query 'system-alerts' (1 items):
[multi-source-logger]   ⚠️  ALERT: HIGH - Database connection pool exhausted
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

Set the Rust log level to debug to see latency metrics:

```bash
RUST_LOG=debug cargo run
```

**Note**: Query results are always printed to stdout. Debug logging only enables additional internal diagnostics and latency measurements.

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
4. Verify console output is being captured (LogReaction writes directly to stdout)
5. Enable debug logging for internal diagnostics: `RUST_LOG=debug`

**Note**: LogReaction outputs directly to stdout using `println!` and does not require any logger initialization (like `env_logger`). If you see startup messages but no query results, the query itself may not be producing results.

### Template Rendering Errors

**Symptoms**: JSON output instead of template output

**Solutions**:
1. Check template syntax (Handlebars format)
2. Verify variable names match available context
3. Look for error logs: `RUST_LOG=debug`
4. Test template with simple expressions first
5. For per-query routes, verify the query ID matches exactly

**Template Priority**: Query-specific routes override defaults. If a query-specific template is set but produces errors, it won't fall back to the default template - it will fall back to JSON output.

### Unexpected Template Output

**Symptoms**: Wrong template being used for a query

**Solutions**:
1. Verify query ID spelling - route lookups are case-sensitive
2. Check that per-query routes are being set for the correct query ID
3. Use `RUST_LOG=debug` to see which templates are being applied
4. Remember the template priority: Query-specific routes > Default template > JSON

**Example**:
```rust
// ❌ Wrong - route won't match due to ID mismatch
.with_query("sensor-data")
.with_route("sensor_data", sensor_config) // Uses underscore instead of hyphen

// ✅ Correct - IDs match
.with_query("sensor-data")
.with_route("sensor-data", sensor_config)
```

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
