# Mock Source

## Overview

The Mock Source is a synthetic data generator for testing, development, and demonstration purposes within Drasi applications. It generates continuous streams of graph node data without requiring external systems, making it ideal for:

- **Testing**: Validate query logic and reactions without external dependencies
- **Prototyping**: Rapidly build and iterate on Drasi applications
- **Demonstrations**: Show Drasi capabilities without infrastructure setup
- **Load Testing**: Generate high-volume data streams for performance testing

### Key Features

- Three data generation modes: Counter, SensorReading, and Generic
- Configurable generation intervals (milliseconds to seconds)
- Realistic sensor behavior (INSERT on first reading, UPDATE thereafter)
- Builder pattern for fluent construction
- Built-in test utilities for unit testing
- Zero external dependencies

## Getting Started

### Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
drasi-source-mock = { path = "path/to/drasi-core/components/sources/mock" }
drasi-lib = { path = "path/to/drasi-core/lib" }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
```

### Minimal Example

```rust
use drasi_source_mock::{MockSource, DataType};
use drasi_lib::Source;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create a counter source with default settings
    let source = MockSource::builder("counter")
        .with_data_type(DataType::Counter)
        .with_interval_ms(1000)
        .build()?;

    // Subscribe to receive events
    let mut rx = source.test_subscribe();

    // Start generating events
    source.start().await?;

    // Receive and print 5 events
    for _ in 0..5 {
        let event = rx.recv().await?;
        println!("Received: {:?}", event);
    }

    source.stop().await?;
    Ok(())
}
```

### Choosing a Data Type

| Data Type | Use When You Need | Event Behavior |
|-----------|-------------------|----------------|
| `Counter` | Sequential, predictable values for testing ordering | Always INSERT |
| `SensorReading` | Realistic IoT simulation with updates to existing entities | INSERT then UPDATE |
| `Generic` | Random data for general testing | Always INSERT |

## Usage Examples

### Basic: Counter Source

```rust
use drasi_source_mock::{MockSource, DataType};

// Counter generates sequential values: 1, 2, 3, ...
let source = MockSource::builder("my-counter")
    .with_data_type(DataType::Counter)
    .with_interval_ms(1000)  // Generate every second
    .build()?;
```

### IoT Simulation: Sensor Readings

```rust
use drasi_source_mock::{MockSource, DataType};

// Simulates 10 sensors reporting temperature and humidity
let source = MockSource::builder("sensors")
    .with_data_type(DataType::sensor_reading(10))
    .with_interval_ms(2000)
    .build()?;
```

### Integration with DrasiLib

```rust
use drasi_source_mock::{MockSource, DataType};
use drasi_lib::{DrasiLib, Query};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create mock source
    let source = MockSource::builder("sensors")
        .with_data_type(DataType::sensor_reading(10))
        .with_interval_ms(1000)
        .build()?;

    // Build Drasi application with the source
    let drasi = DrasiLib::builder()
        .with_id("my-app")
        .with_source(source)
        .build()
        .await?;

    // Add a query that filters high temperature readings
    let query = Query::cypher("high-temp")
        .query("MATCH (s:SensorReading) WHERE s.temperature > 25.0 RETURN s")
        .from_source("sensors")
        .build();

    drasi.add_query(query).await?;
    drasi.start().await?;

    Ok(())
}
```

### Unit Testing

```rust
use drasi_source_mock::{MockSource, MockSourceConfig, DataType};

#[tokio::test]
async fn test_sensor_events() {
    let source = MockSource::builder("test")
        .with_data_type(DataType::sensor_reading(5))
        .with_interval_ms(50)
        .build()
        .unwrap();

    // Subscribe to events directly (bypasses DrasiLib)
    let mut rx = source.test_subscribe();

    source.start().await.unwrap();

    // Receive events
    let event = rx.recv().await.unwrap();
    // Assert on event properties...

    source.stop().await.unwrap();
}
```

## Data Types

### Counter

Generates sequentially numbered nodes starting from 1.

```
Label: Counter
Element ID: counter_{sequence}  (e.g., counter_1, counter_2)
Properties:
  - value: Integer (1, 2, 3, ...)
  - timestamp: String (RFC3339)
```

**Example output:**
```json
{
  "id": "counter_42",
  "labels": ["Counter"],
  "properties": { "value": 42, "timestamp": "2025-12-05T10:30:45Z" }
}
```

### SensorReading

Simulates IoT sensors with temperature and humidity readings.

```
Label: SensorReading
Element ID: sensor_{sensor_id}  (e.g., sensor_0, sensor_3)
Properties:
  - sensor_id: String
  - temperature: Float (20.0 - 30.0)
  - humidity: Float (40.0 - 60.0)
  - timestamp: String (RFC3339)
```

**Key behavior**: First reading for each sensor → INSERT. Subsequent readings → UPDATE.

**Example output:**
```json
{
  "id": "sensor_2",
  "labels": ["SensorReading"],
  "properties": {
    "sensor_id": "sensor_2",
    "temperature": 24.7,
    "humidity": 52.1,
    "timestamp": "2025-12-05T10:30:45Z"
  }
}
```

### Generic

Generates nodes with random integer values.

```
Label: Generic
Element ID: generic_{sequence}  (e.g., generic_1, generic_2)
Properties:
  - value: Integer (random i32)
  - message: String ("Generic mock data")
  - timestamp: String (RFC3339)
```

**Example output:**
```json
{
  "id": "generic_1",
  "labels": ["Generic"],
  "properties": { "value": -1847392047, "message": "Generic mock data", "timestamp": "2025-12-05T10:30:45Z" }
}
```

## Configuration

### Builder Pattern (Recommended)

```rust
use drasi_source_mock::{MockSource, DataType};
use drasi_lib::channels::DispatchMode;

let source = MockSource::builder("my-source")
    .with_data_type(DataType::sensor_reading(10))  // 10 sensors
    .with_interval_ms(1000)                         // 1 second interval
    .with_dispatch_mode(DispatchMode::Channel)      // Backpressure support
    .with_dispatch_buffer_capacity(2000)            // Buffer size
    .with_auto_start(true)                          // Start when added to DrasiLib
    .build()?;
```

### Config Struct

For configuration-file-driven scenarios:

```rust
use drasi_source_mock::{MockSource, MockSourceConfig, DataType};

let config = MockSourceConfig {
    data_type: DataType::sensor_reading(10),
    interval_ms: 1000,
};

let source = MockSource::new("my-source", config)?;
```

### Options Reference

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `id` | `String` | Required | Unique source identifier |
| `data_type` | `DataType` | `Generic` | Data generation mode |
| `interval_ms` | `u64` | `5000` | Milliseconds between events (min: 1) |
| `dispatch_mode` | `DispatchMode` | `Channel` | `Channel` (backpressure) or `Broadcast` |
| `dispatch_buffer_capacity` | `usize` | `1000` | Event buffer size |
| `auto_start` | `bool` | `true` | Auto-start when added to DrasiLib |

### Validation

Configuration is validated on construction. Errors occur if:
- `interval_ms` is 0
- `sensor_count` is 0 (for SensorReading mode)

## Testing Utilities

### `test_subscribe()`

Subscribe directly to source events, bypassing DrasiLib:

```rust
let source = MockSource::builder("test").build()?;
let mut rx = source.test_subscribe();

source.start().await?;

while let Ok(event) = rx.recv().await {
    // Process event
}
```

### `inject_event()`

Inject custom events for deterministic testing:

```rust
use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange};
use std::sync::Arc;

let source = MockSource::builder("test").build()?;
let mut rx = source.test_subscribe();

// Create custom element
let reference = ElementReference::new("test", "custom_1");
let metadata = ElementMetadata {
    reference,
    labels: Arc::from(vec![Arc::from("Custom")]),
    effective_from: 0,
};
let mut properties = ElementPropertyMap::new();
properties.insert("value", drasi_core::models::ElementValue::Integer(999));

let element = Element::Node { metadata, properties };

// Inject and receive
source.inject_event(SourceChange::Insert { element }).await?;
let event = rx.recv().await?;
```

## Implementation Details

### Lifecycle

```
Stopped → Starting → Running → Stopping → Stopped
```

- **start()**: Spawns a Tokio task that generates events at the configured interval
- **stop()**: Aborts the generation task and waits for completion
- **status()**: Returns current `ComponentStatus`

### Event Generation

The source runs an internal Tokio task that:
1. Waits for the configured interval
2. Generates a node based on `data_type`
3. Dispatches the event to all subscribers
4. Repeats until stopped

### Dispatch Modes

| Mode | Behavior | Use Case |
|------|----------|----------|
| `Channel` | Isolated channel per subscriber with backpressure | Production, reliable delivery |
| `Broadcast` | Shared channel, no backpressure, may drop events | High throughput, lossy acceptable |

## Performance

### Recommended Intervals

| Use Case | Interval | Events/sec |
|----------|----------|------------|
| Load testing | 10-50ms | 20-100 |
| Development | 100-500ms | 2-10 |
| Demos | 1000-3000ms | 0.3-1 |

**Warning**: Intervals below 10ms may saturate CPU with complex queries.

### Memory

- ~200-500 bytes per event
- Single Tokio task per source
- Buffer memory = `dispatch_buffer_capacity` × event size

## Troubleshooting

### No Events Received

1. Verify source is started: `source.start().await?`
2. Check status: `source.status().await` should be `Running`
3. Ensure subscriber is registered before starting

### Wrong Data Type

Check the `data_type` property:
```rust
let props = source.properties();
println!("Data type: {:?}", props.get("data_type"));
```

### Query Returns Empty

Verify label and property names match the data type:

| Data Type | Label | Properties |
|-----------|-------|------------|
| Counter | `Counter` | `value`, `timestamp` |
| SensorReading | `SensorReading` | `sensor_id`, `temperature`, `humidity`, `timestamp` |
| Generic | `Generic` | `value`, `message`, `timestamp` |

### Validation Errors

```
Error: interval_ms cannot be 0
```
→ Use a positive interval (minimum 1)

```
Error: sensor_count cannot be 0
```
→ Use `DataType::sensor_reading(n)` where n ≥ 1

## Limitations

- Generates nodes only (no relationships/edges)
- Fixed schemas per data type (not customizable)
- Only SensorReading supports UPDATE events
- State resets on restart (counter, seen sensors)
- Randomness is not cryptographically secure

## Related Documentation

- Source trait: `lib/src/sources/mod.rs`
- Test examples: `components/sources/mock/src/tests.rs`
