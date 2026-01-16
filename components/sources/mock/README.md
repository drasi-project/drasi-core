# Mock Source

## Overview

The Mock Source is a synthetic data generator plugin designed for testing, development, and demonstration purposes within the Drasi data processing platform. It generates continuous streams of graph node data without requiring any external systems or databases, making it ideal for rapid prototyping, testing query logic, and demonstrating Drasi capabilities.

### Key Capabilities

- **Three Built-in Data Generation Modes**: Counter, Sensor, and Generic data types
- **Configurable Generation Intervals**: Control data emission rate from milliseconds to seconds
- **Zero External Dependencies**: No databases, APIs, or external services required
- **Async Task-Based Generation**: Runs as a Tokio task with precise interval timing
- **Builder Pattern Support**: Fluent API for easy source construction
- **Label-Based Bootstrap Filtering**: Supports bootstrap requests with label-based filtering
- **Test Utilities**: Built-in methods for injecting events and subscribing to data in tests

### Use Cases

**Testing and Development**
- Test continuous query logic without setting up external databases
- Validate query filtering, aggregation, and transformation logic
- Develop and test reactions in isolation
- Prototype new features quickly with predictable data

**Demonstrations and Presentations**
- Show Drasi capabilities without infrastructure setup
- Create predictable, repeatable demo scenarios
- Demonstrate real-time data processing and continuous queries

**Load Testing**
- Generate continuous data streams for performance testing
- Test system behavior under sustained load with configurable rates
- Validate scaling characteristics and backpressure handling

**Integration Testing**
- Test multi-source pipelines with predictable data
- Validate data routing between components
- Test bootstrap and streaming data integration

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides a fluent API for constructing MockSource instances with compile-time validation:

```rust
use drasi_source_mock::MockSource;
use drasi_lib::channels::DispatchMode;

// Basic construction with defaults
let source = MockSource::builder("my-source")
    .build()?;

// Full configuration with all options
let source = MockSource::builder("sensor-source")
    .with_data_type("sensor")
    .with_interval_ms(1000)
    .with_dispatch_mode(DispatchMode::Channel)
    .with_dispatch_buffer_capacity(2000)
    .with_bootstrap_provider(my_bootstrap_provider)
    .with_auto_start(true)
    .build()?;

// Counter source for testing
let counter = MockSource::builder("counter")
    .with_data_type("counter")
    .with_interval_ms(500)
    .build()?;
```

### Config Struct Approach

For programmatic or configuration-file-driven scenarios:

```rust
use drasi_source_mock::{MockSource, MockSourceConfig};

// Using MockSourceConfig
let config = MockSourceConfig {
    data_type: "sensor".to_string(),
    interval_ms: 1000,
};

let source = MockSource::new("sensor-source", config)?;

// With custom dispatch settings
let source = MockSource::with_dispatch(
    "sensor-source",
    config,
    Some(DispatchMode::Channel),
    Some(2000),
)?;
```

### Configuration Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `id` | Unique identifier for the source instance | `String` | Any non-empty string | **(Required)** |
| `data_type` | Type of synthetic data to generate | `String` | `"counter"`, `"sensor"`, `"generic"` | `"generic"` |
| `interval_ms` | Interval between data generation events in milliseconds | `u64` | Any positive integer (minimum 1) | `5000` |
| `dispatch_mode` | Event dispatch mode for subscribers | `DispatchMode` | `Channel` (isolated with backpressure), `Broadcast` (shared, no backpressure) | `Channel` |
| `dispatch_buffer_capacity` | Buffer size for dispatch channels | `usize` | Any positive integer | `1000` |
| `bootstrap_provider` | Bootstrap provider for initial data delivery | `Box<dyn BootstrapProvider>` | Any bootstrap provider implementation | `None` |
| `auto_start` | Whether to start automatically when added to DrasiLib | `bool` | `true`, `false` | `true` |

**Configuration Validation:**

The `MockSourceConfig::validate()` method checks:
- `data_type` is one of the valid types: `"counter"`, `"sensor"`, or `"generic"`
- `interval_ms` is greater than 0 (non-zero interval required)

## Input Schema

The MockSource generates data internally and does not consume external input. However, it produces graph nodes with the following schemas based on the configured `data_type`:

### Counter Mode (`data_type: "counter"`)

**Generated Node Schema:**
```
Label: Counter
Element ID Format: counter_{sequence}
Properties:
  - value: Integer (sequential counter starting from 1)
  - timestamp: String (RFC3339 formatted UTC timestamp)
```

**Example Node:**
```json
{
  "id": "counter_42",
  "labels": ["Counter"],
  "properties": {
    "value": 42,
    "timestamp": "2025-12-05T10:30:45.123456789Z"
  }
}
```

**Characteristics:**
- Sequential integer values starting from 1
- Increments by 1 on each generation
- Predictable, deterministic sequence
- Useful for testing ordering and sequential processing

### Sensor Mode (`data_type: "sensor"`)

**Generated Node Schema:**
```
Label: SensorReading
Element ID Format: reading_{sensor_id}_{sequence}
Properties:
  - sensor_id: String (randomly selected: "sensor_0" to "sensor_4")
  - temperature: Float (random value between 20.0 and 30.0)
  - humidity: Float (random value between 40.0 and 60.0)
  - timestamp: String (RFC3339 formatted UTC timestamp)
```

**Example Node:**
```json
{
  "id": "reading_2_42",
  "labels": ["SensorReading"],
  "properties": {
    "sensor_id": "sensor_2",
    "temperature": 24.73,
    "humidity": 52.18,
    "timestamp": "2025-12-05T10:30:45.123456789Z"
  }
}
```

**Characteristics:**
- Simulates 5 different IoT sensors (sensor_0 through sensor_4)
- Randomized sensor selection on each generation
- Temperature range: 20.0°C to 30.0°C
- Humidity range: 40% to 60%
- Useful for testing IoT scenarios, aggregations, and filtering

### Generic Mode (`data_type: "generic"`)

**Generated Node Schema:**
```
Label: Generic
Element ID Format: generic_{sequence}
Properties:
  - value: Integer (random 32-bit signed integer)
  - message: String (fixed: "Generic mock data")
  - timestamp: String (RFC3339 formatted UTC timestamp)
```

**Example Node:**
```json
{
  "id": "generic_42",
  "labels": ["Generic"],
  "properties": {
    "value": -1847392047,
    "message": "Generic mock data",
    "timestamp": "2025-12-05T10:30:45.123456789Z"
  }
}
```

**Characteristics:**
- Random integer values (full i32 range: -2,147,483,648 to 2,147,483,647)
- Fixed message string for consistency
- Default mode when data_type is not specified
- General-purpose testing and development

## Usage Examples

### Basic Usage

```rust
use drasi_source_mock::MockSource;
use drasi_lib::Source;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create a counter source
    let counter_source = Arc::new(
        MockSource::builder("counter-source")
            .with_data_type("counter")
            .with_interval_ms(1000)
            .build()?
    );

    // Start the source
    counter_source.start().await?;

    // Let it run for a while
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Stop the source
    counter_source.stop().await?;

    Ok(())
}
```

### Integration with DrasiLib

```rust
use drasi_source_mock::MockSource;
use drasi_lib::DrasiLib;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create Drasi instance
    let mut drasi = DrasiLib::builder("my-app")
        .build()
        .await?;

    // Create and add mock source
    let sensor_source = Arc::new(
        MockSource::builder("sensors")
            .with_data_type("sensor")
            .with_interval_ms(2000)
            .build()?
    );

    drasi.add_source(sensor_source).await?;

    // Add a continuous query
    let query = drasi_lib::Query::cypher("high-temp")
        .query("MATCH (s:SensorReading) WHERE s.temperature > 25.0 RETURN s")
        .from_source("sensors")
        .build();

    drasi.add_query(query).await?;

    // Run the system
    drasi.run().await?;

    Ok(())
}
```

### Testing with Mock Source

```rust
use drasi_source_mock::{MockSource, MockSourceConfig};
use drasi_lib::channels::SourceEvent;
use drasi_lib::Source;

#[tokio::test]
async fn test_sensor_data_generation() {
    // Create sensor source
    let config = MockSourceConfig {
        data_type: "sensor".to_string(),
        interval_ms: 100,
    };
    let source = MockSource::new("test-sensor", config).unwrap();

    // Subscribe to receive events
    let mut rx = source.test_subscribe();

    // Start generating data
    source.start().await.unwrap();

    // Collect some events
    let mut events = Vec::new();
    for _ in 0..5 {
        if let Ok(event) = rx.recv().await {
            events.push(event);
        }
    }

    // Stop the source
    source.stop().await.unwrap();

    // Verify we received events
    assert_eq!(events.len(), 5);
}
```

### Manual Event Injection (Testing)

```rust
use drasi_source_mock::MockSource;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap,
    ElementReference, SourceChange, ElementValue
};
use std::sync::Arc;

#[tokio::test]
async fn test_event_injection() {
    let source = MockSource::builder("test-source")
        .with_data_type("counter")
        .build()
        .unwrap();

    let mut rx = source.test_subscribe();

    // Manually inject a custom event
    let element_id = "custom_1";
    let reference = ElementReference::new("test-source", element_id);

    let mut properties = ElementPropertyMap::new();
    properties.insert("value", ElementValue::Integer(999));

    let metadata = ElementMetadata {
        reference,
        labels: Arc::from(vec![Arc::from("Custom")]),
        effective_from: 1234567890,
    };

    let element = Element::Node { metadata, properties };
    let change = SourceChange::Insert { element };

    // Inject the event
    source.inject_event(change).await.unwrap();

    // Receive and verify
    let event = rx.recv().await.unwrap();
    // Verify event contains custom data
}
```

### With Bootstrap Provider

```rust
use drasi_source_mock::MockSource;
use drasi_lib::bootstrap::{BootstrapProvider, BootstrapRequest};
use async_trait::async_trait;

// Custom bootstrap provider
struct MyBootstrapProvider;

#[async_trait]
impl BootstrapProvider for MyBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
    ) -> anyhow::Result<Vec<drasi_core::models::SourceChange>> {
        // Return initial data based on request labels
        Ok(vec![/* initial changes */])
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let source = MockSource::builder("sensor-source")
        .with_data_type("sensor")
        .with_interval_ms(1000)
        .with_bootstrap_provider(MyBootstrapProvider)
        .build()?;

    // Bootstrap will be called when queries subscribe
    source.start().await?;

    Ok(())
}
```

### Different Dispatch Modes

```rust
use drasi_source_mock::MockSource;
use drasi_lib::channels::DispatchMode;

// Channel mode - isolated channels per subscriber with backpressure
let channel_source = MockSource::builder("channel-source")
    .with_data_type("sensor")
    .with_dispatch_mode(DispatchMode::Channel)
    .with_dispatch_buffer_capacity(5000)
    .build()?;

// Broadcast mode - shared channel, no backpressure
// Events may be dropped if subscribers can't keep up
let broadcast_source = MockSource::builder("broadcast-source")
    .with_data_type("counter")
    .with_dispatch_mode(DispatchMode::Broadcast)
    .with_dispatch_buffer_capacity(100)
    .build()?;
```

## Implementation Details

### Data Generation Mechanism

The MockSource runs an internal Tokio task that:

1. **Interval Timer**: Uses `tokio::time::interval` for precise timing
2. **Status Checking**: Monitors component status and stops when not Running
3. **Sequence Counter**: Maintains a sequence number starting from 1
4. **Data Generation**: Creates nodes based on `data_type` configuration
5. **Event Dispatch**: Publishes via SourceBase dispatcher system
6. **Profiling Metadata**: Includes timestamps for performance tracking

**Generation Loop:**
```
Start → Set Interval → Tick → Check Status → Generate Data →
Dispatch Event → Increment Sequence → Tick → ...
```

### Element ID Generation

All generated nodes have predictable element IDs:

- **Counter**: `counter_{sequence}` (e.g., `counter_1`, `counter_2`, ...)
- **Sensor**: `reading_{sensor_id}_{sequence}` (e.g., `reading_2_42`)
- **Generic**: `generic_{sequence}` (e.g., `generic_1`, `generic_2`, ...)

The sequence number increments with each generation cycle, providing traceability and ordering.

### Timestamp Generation

All modes include an RFC3339 formatted timestamp:

```
2025-12-05T10:30:45.123456789Z
```

**Timestamp Strategy:**
- Primary: `chrono::Utc::now().to_rfc3339()` for timestamp property
- Metadata: Nanosecond precision using `SystemTime::duration_since(UNIX_EPOCH)`
- Fallback: If nanosecond timestamp fails, uses millisecond precision × 1,000,000

### Randomization Details

**Sensor Mode:**
- Temperature: `20.0 + rand::random::<f64>() * 10.0` → [20.0, 30.0)
- Humidity: `40.0 + rand::random::<f64>() * 20.0` → [40.0, 60.0)
- Sensor ID: `rand::random::<u32>() % 5` → {0, 1, 2, 3, 4}

**Generic Mode:**
- Value: `rand::random::<i32>()` → Full i32 range

**Note**: Uses the `rand` crate's default RNG, not cryptographically secure.

### Lifecycle Management

**Status Transitions:**
```
Stopped → Starting → Running → Stopping → Stopped
```

**Start Process:**
1. Set status to Starting
2. Send component event
3. Spawn generation task
4. Set status to Running
5. Send success event

**Stop Process:**
1. Set status to Stopping
2. Send component event
3. Abort generation task
4. Wait for task completion
5. Set status to Stopped
6. Send success event

### Label-Based Bootstrap

When queries subscribe with bootstrap enabled:

1. Query provides required node/relation labels
2. Bootstrap provider (if configured) receives label filters
3. Provider returns initial data matching labels
4. Initial data sent before continuous streaming begins

**Default Behavior**: If no bootstrap provider is configured, bootstrap completes immediately with no initial data.

## Testing Utilities

### `test_subscribe()`

Creates a test subscription that receives all generated events:

```rust
let source = MockSource::builder("test").build()?;
let mut rx = source.test_subscribe();

source.start().await?;

// Receive events
while let Ok(event) = rx.recv().await {
    // Process event
}
```

**Returns**: `Box<dyn ChangeReceiver<SourceEventWrapper>>`

### `inject_event()`

Manually injects a custom event for testing:

```rust
let source = MockSource::builder("test").build()?;
let mut rx = source.test_subscribe();

let change = SourceChange::Insert { element };
source.inject_event(change).await?;

// Event will be received by subscribers
let event = rx.recv().await?;
```

**Use Cases:**
- Testing specific edge cases
- Simulating error conditions
- Testing event processing logic
- Creating deterministic test scenarios

## Performance Characteristics

### Generation Rate

| Interval Setting | Events/Second | Typical Use Case |
|------------------|---------------|------------------|
| 10-50ms | 20-100 | High-volume load testing |
| 100-500ms | 2-10 | Rapid testing, development |
| 1000-3000ms | 0.33-1 | Demos, presentations |
| 5000-10000ms | 0.1-0.2 | Slow background generation |

**Default**: 5000ms (0.2 events/second)

### Memory Overhead

- **Per Event**: Minimal (~200-500 bytes per node)
- **Task Overhead**: Single Tokio task per source
- **No Persistence**: Events are not stored, only generated and dispatched
- **Channel Buffers**: Configurable (default 1000 events)

### Throughput Recommendations

**Testing**: 100-1000ms intervals
- Fast enough to catch issues quickly
- Not overwhelming for debugging

**Demonstrations**: 1000-3000ms intervals
- Visible data flow without overwhelming viewers
- Comfortable pace for explanations

**Load Testing**: 10-100ms intervals
- High-volume data generation
- Tests backpressure and buffer handling
- May impact system performance with complex queries

**Warning**: Very short intervals (<10ms) can saturate CPU and memory depending on query complexity and reaction processing time.

### Dispatch Mode Performance

**Channel Mode (Default)**:
- Isolated channel per subscriber
- Backpressure applied when subscriber is slow
- Zero message loss guarantee
- Higher memory usage with many subscribers

**Broadcast Mode**:
- Single shared channel for all subscribers
- No backpressure (fast path)
- Slow subscribers may drop messages
- Lower memory overhead

## Known Limitations

1. **Insert-Only Operations**: Only generates insert events, never updates or deletes
2. **Node-Only Data**: Only generates nodes, not graph relationships/edges
3. **Fixed Schemas**: Each mode has a predefined schema that cannot be customized
4. **No Custom Properties**: Cannot add additional properties beyond the schema
5. **Predictable Randomness**: Not cryptographically secure (uses standard `rand` crate)
6. **Single Label Per Mode**: Each mode generates nodes with exactly one label
7. **No State Persistence**: Sequence counter resets on restart
8. **No Relationship Generation**: Cannot generate edges between nodes

## Troubleshooting

### No Data Being Generated

**Symptoms:**
- Source status shows Running but no events received
- Queries return no results

**Checks:**
1. Verify source has been started: `source.start().await?`
2. Check status: `source.status().await` should be `ComponentStatus::Running`
3. Verify interval is reasonable (not hours/days)
4. Check logs for "Mock source started successfully" message
5. Ensure subscribers are properly registered

**Debug Commands:**
```rust
// Check status
let status = source.status().await;
println!("Source status: {:?}", status);

// Check properties
let props = source.properties();
println!("Data type: {:?}", props.get("data_type"));
println!("Interval: {:?}", props.get("interval_ms"));
```

### Wrong Data Type Generated

**Symptoms:**
- Unexpected node labels in query results
- Missing expected properties

**Checks:**
1. Verify `data_type` spelling is exact (case-sensitive)
2. Valid values: `"counter"`, `"sensor"`, `"generic"`
3. Invalid/missing values default to `"generic"`
4. Check configuration was applied correctly

**Verification:**
```rust
let props = source.properties();
assert_eq!(
    props.get("data_type"),
    Some(&serde_json::Value::String("sensor".to_string()))
);
```

### Query Returns No Results

**Symptoms:**
- Source is generating data but queries are empty

**Common Causes:**

**Wrong Label:**
```cypher
-- Wrong: Looking for wrong label
MATCH (n:Sensor) RETURN n  // Should be SensorReading

-- Correct:
MATCH (n:SensorReading) RETURN n
```

**Wrong Property Names:**
```cypher
-- Wrong: Property name typo
MATCH (s:SensorReading) WHERE s.temp > 25 RETURN s  // Should be temperature

-- Correct:
MATCH (s:SensorReading) WHERE s.temperature > 25 RETURN s
```

**Impossible Filter:**
```cypher
-- Wrong: Temperature range is 20-30, this filters everything
MATCH (s:SensorReading) WHERE s.temperature > 100 RETURN s

-- Correct: Use realistic range
MATCH (s:SensorReading) WHERE s.temperature > 25 RETURN s
```

### Configuration Validation Errors

**Invalid Data Type:**
```
Error: Validation error: data_type 'sensors' is not valid.
Valid options are: counter, sensor, generic
```

**Solution**: Use exact spelling: `"counter"`, `"sensor"`, or `"generic"`

**Zero Interval:**
```
Error: Validation error: interval_ms cannot be 0.
Please specify a positive interval in milliseconds (minimum 1)
```

**Solution**: Use positive interval (minimum 1ms)

### High CPU Usage

**Symptoms:**
- MockSource consuming significant CPU
- System slowdown

**Common Causes:**
1. Very short interval (<10ms) generating too many events
2. Complex queries processing each event
3. Too many concurrent mock sources

**Solutions:**
```rust
// Increase interval for lower CPU usage
let source = MockSource::builder("sensor")
    .with_interval_ms(1000)  // Was: 10ms
    .build()?;

// Use broadcast mode to reduce dispatch overhead
let source = MockSource::builder("sensor")
    .with_dispatch_mode(DispatchMode::Broadcast)
    .build()?;
```

## Related Components

### Dependencies

- **drasi-lib**: Core Drasi library providing Source trait and channel infrastructure
- **drasi-core**: Core models (Element, SourceChange, ElementValue, etc.)
- **tokio**: Async runtime for task execution and interval timing
- **chrono**: Timestamp generation and formatting
- **rand**: Random value generation for sensor and generic modes
- **serde/serde_json**: Configuration serialization

### Related Documentation

- **Source Plugin Guide**: `/Users/allenjones/dev/agentofreality/drasi/drasi-server/drasi-core/lib/src/sources/README.md`
- **Bootstrap Providers**: `/Users/allenjones/dev/agentofreality/drasi/drasi-server/drasi-core/lib/src/bootstrap/README.md`
- **Channel System**: `/Users/allenjones/dev/agentofreality/drasi/drasi-server/drasi-core/lib/src/channels/README.md`
- **SourceBase**: `/Users/allenjones/dev/agentofreality/drasi/drasi-server/drasi-core/lib/src/sources/base.rs`

### Example Projects

See the test suite in `src/tests.rs` for comprehensive usage examples including:
- Construction patterns
- Lifecycle management
- Event generation verification
- Builder API usage
- Configuration serialization

## Changelog

### Version 0.2.0
- Initial release as separate plugin crate
- Three data generation modes: counter, sensor, generic
- Builder pattern support
- Bootstrap provider integration
- Test utilities (inject_event, test_subscribe)
- Configurable dispatch modes and buffer capacity
