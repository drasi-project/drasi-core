# Mock Source

## Overview

The Mock Source is a built-in synthetic data generator designed for testing, development, and demonstration purposes. It generates continuous streams of graph node data without requiring any external systems or databases. The mock source runs as an internal Tokio task and publishes data changes at configurable intervals.

**Key Features:**
- Three built-in data generation modes: Counter, Sensor, and Generic
- Configurable generation intervals
- No external dependencies required
- Ideal for testing continuous query logic
- Suitable for demos and presentations
- Perfect for development without external data sources

## Use Cases

### Testing and Development
- Test continuous query logic without setting up external databases
- Validate query filtering and transformation logic
- Develop and test reactions in isolation
- Prototype new features quickly

### Demonstrations and Presentations
- Show Drasi capabilities without infrastructure setup
- Create predictable, repeatable demo scenarios
- Demonstrate real-time data processing

### Load Testing
- Generate continuous data streams for performance testing
- Test system behavior under sustained load
- Validate scaling characteristics

### Integration Testing
- Test multi-source pipelines
- Validate data routing between components
- Test bootstrap and streaming integration

## Configuration

### Configuration Settings

The Mock Source supports the following configuration settings:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `id` | String | Unique identifier for the source | Any string | **(Required)** |
| `source_type` | String | Source type discriminator | "mock" | **(Required)** |
| `auto_start` | Boolean | Whether to automatically start this source | true, false | `true` |
| `data_type` | String | Type of synthetic data to generate | `"counter"`, `"sensor"`, `"generic"` | `"generic"` |
| `interval_ms` | Integer (u64) | Interval between data generation events in milliseconds | Any positive integer (1 - 2^64) | `5000` |
| `dispatch_mode` | String (Optional) | Event dispatch mode: "channel" (isolated channels per subscriber with backpressure, zero message loss) or "broadcast" (shared channel, no backpressure, possible message loss) | "channel", "broadcast" | `"channel"` |
| `dispatch_buffer_capacity` | Integer (Optional) | Buffer size for dispatch channel | Any positive integer | `1000` |
| `bootstrap_provider` | Object (Optional) | Bootstrap provider configuration | See Bootstrap Providers section | `None` |

**Note**: The Mock Source does not use any common configuration types from `config/common.rs` (such as SslMode or TableKeyConfig).

## Data Generation Modes

### Counter Mode

Generates incrementing counter nodes with sequential values.

**Node Label:** `Counter`

**Properties Generated:**
- `value` (Integer): Sequential counter starting from 1
- `timestamp` (String): RFC3339 formatted UTC timestamp

**Element ID Format:** `counter_{sequence_number}`

**Example Node:**
```json
{
  "labels": ["Counter"],
  "properties": {
    "value": 42,
    "timestamp": "2025-10-19T10:30:45.123456789Z"
  }
}
```

**Use Cases:**
- Testing sequential data processing
- Validating ordering logic
- Simple increment-based queries

### Sensor Mode

Generates simulated sensor readings with randomized temperature and humidity values from a pool of 5 sensors (sensor_0 through sensor_4).

**Node Label:** `SensorReading`

**Properties Generated:**
- `sensor_id` (String): Randomly selected from `"sensor_0"` to `"sensor_4"`
- `temperature` (Float): Random value between 20.0 and 30.0
- `humidity` (Float): Random value between 40.0 and 60.0
- `timestamp` (String): RFC3339 formatted UTC timestamp

**Element ID Format:** `reading_{sensor_id}_{sequence_number}`

**Example Node:**
```json
{
  "labels": ["SensorReading"],
  "properties": {
    "sensor_id": "sensor_2",
    "temperature": 24.73,
    "humidity": 52.18,
    "timestamp": "2025-10-19T10:30:45.123456789Z"
  }
}
```

**Use Cases:**
- Testing IoT-style data processing
- Demonstrating aggregation queries
- Testing range-based filters

### Generic Mode

Generates nodes with random integer values and a static message property.

**Node Label:** `Generic`

**Properties Generated:**
- `value` (Integer): Random 32-bit integer
- `message` (String): Fixed string `"Generic mock data"`
- `timestamp` (String): RFC3339 formatted UTC timestamp

**Element ID Format:** `generic_{sequence_number}`

**Example Node:**
```json
{
  "labels": ["Generic"],
  "properties": {
    "value": -1847392047,
    "message": "Generic mock data",
    "timestamp": "2025-10-19T10:30:45.123456789Z"
  }
}
```

**Use Cases:**
- General-purpose testing
- Property selection queries
- Default behavior when data_type is not specified

## Configuration Examples

### YAML Configuration

#### Counter Source
```yaml
sources:
  - id: "counter-source"
    source_type: "mock"
    auto_start: true
    dispatch_mode: "channel"  # Isolated channels with backpressure
    dispatch_buffer_capacity: 1000
    data_type: "counter"
    interval_ms: 2000  # Generate every 2 seconds
```

#### Sensor Source
```yaml
sources:
  - id: "sensor-source"
    source_type: "mock"
    auto_start: true
    data_type: "sensor"
    interval_ms: 1500  # Generate every 1.5 seconds
```

#### Generic Source
```yaml
sources:
  - id: "generic-source"
    source_type: "mock"
    auto_start: true
    data_type: "generic"
    interval_ms: 5000  # Generate every 5 seconds
```

#### With Bootstrap Provider
```yaml
sources:
  - id: "mock-with-bootstrap"
    source_type: "mock"
    auto_start: true
    data_type: "sensor"
    interval_ms: 2000
    bootstrap_provider:
      type: scriptfile
      file_paths:
        - "examples/data/sensor_initial.jsonl"
```

### JSON Configuration

#### Counter Source
```json
{
  "sources": [
    {
      "id": "counter-source",
      "source_type": "mock",
      "auto_start": true,
      "data_type": "counter",
      "interval_ms": 2000
    }
  ]
}
```

#### Sensor Source
```json
{
  "sources": [
    {
      "id": "sensor-source",
      "source_type": "mock",
      "auto_start": true,
      "data_type": "sensor",
      "interval_ms": 1500
    }
  ]
}
```

### Rust Programmatic Configuration

Using the builder API:

```rust
use drasi_server_core::Source;
use serde_json::json;

// Counter source
let counter_source = Source::mock("counter-source")
    .with_property("data_type", json!("counter"))
    .with_property("interval_ms", json!(2000))
    .auto_start(true)
    .build();

// Sensor source
let sensor_source = Source::mock("sensor-source")
    .with_property("data_type", json!("sensor"))
    .with_property("interval_ms", json!(1500))
    .auto_start(true)
    .build();

// Generic source
let generic_source = Source::mock("generic-source")
    .with_property("data_type", json!("generic"))
    .with_property("interval_ms", json!(5000))
    .auto_start(true)
    .build();
```

Using direct configuration:

```rust
use drasi_server_core::config::SourceConfig;
use std::collections::HashMap;
use serde_json::json;

let mut properties = HashMap::new();
properties.insert("data_type".to_string(), json!("sensor"));
properties.insert("interval_ms".to_string(), json!(1000));

let config = SourceConfig {
    id: "test-sensor".to_string(),
    source_type: "mock".to_string(),
    auto_start: true,
    properties,
    bootstrap_provider: None,
};
```

## Generated Data Schemas

### Counter Data Schema

```
Label: Counter
Properties:
  - value: Integer (sequential, starts at 1)
  - timestamp: String (RFC3339 format)
Element ID: counter_{sequence}
```

**Example Query:**
```cypher
MATCH (c:Counter)
WHERE c.value > 5
RETURN c.value, c.timestamp
```

### Sensor Data Schema

```
Label: SensorReading
Properties:
  - sensor_id: String (sensor_0 to sensor_4)
  - temperature: Float (20.0 to 30.0)
  - humidity: Float (40.0 to 60.0)
  - timestamp: String (RFC3339 format)
Element ID: reading_{sensor_id}_{sequence}
```

**Example Queries:**
```cypher
// Find high temperature readings
MATCH (s:SensorReading)
WHERE s.temperature > 25.0
RETURN s

// Find readings from specific sensor
MATCH (s:SensorReading)
WHERE s.sensor_id = "sensor_2"
RETURN s

// Find high humidity readings
MATCH (s:SensorReading)
WHERE s.humidity > 55.0
RETURN s.sensor_id, s.humidity
```

### Generic Data Schema

```
Label: Generic
Properties:
  - value: Integer (random 32-bit integer)
  - message: String (always "Generic mock data")
  - timestamp: String (RFC3339 format)
Element ID: generic_{sequence}
```

**Example Query:**
```cypher
MATCH (g:Generic)
RETURN g.value, g.timestamp
```

## Example Queries

### Counter Mode Queries

```cypher
// Return all counter values
MATCH (c:Counter) RETURN c

// Filter counters greater than threshold
MATCH (c:Counter)
WHERE c.value > 10
RETURN c.value

// Return only the value property
MATCH (c:Counter)
RETURN c.value
```

### Sensor Mode Queries

```cypher
// Return all sensor readings
MATCH (s:SensorReading) RETURN s

// Find temperature anomalies
MATCH (s:SensorReading)
WHERE s.temperature > 28.0 OR s.temperature < 22.0
RETURN s.sensor_id, s.temperature

// Monitor specific sensor
MATCH (s:SensorReading)
WHERE s.sensor_id = "sensor_3"
RETURN s.temperature, s.humidity, s.timestamp

// Combined conditions
MATCH (s:SensorReading)
WHERE s.temperature > 25.0 AND s.humidity > 50.0
RETURN s
```

### Generic Mode Queries

```cypher
// Return all generic data
MATCH (g:Generic) RETURN g

// Filter by value range
MATCH (g:Generic)
WHERE g.value > 0
RETURN g.value, g.timestamp

// Select specific properties
MATCH (g:Generic)
RETURN g.value, g.message
```

## Complete Integration Example

Here's a complete example showing a mock source with a query and reaction:

```yaml
server:
  id: "mock-example"

sources:
  - id: "sensor-data"
    source_type: "mock"
    auto_start: true
    data_type: "sensor"
    interval_ms: 2000

queries:
  - id: "high-temperature-alert"
    query: "MATCH (s:SensorReading) WHERE s.temperature > 26.0 RETURN s"
    query_language: "cypher"
    sources: ["sensor-data"]
    auto_start: true

reactions:
  - id: "temperature-logger"
    reaction_type: "log"
    queries: ["high-temperature-alert"]
    auto_start: true
    properties:
      log_level: "warn"
```

## Use in Testing

### Unit Tests

```rust
use drasi_server_core::sources::MockSource;
use drasi_server_core::config::SourceConfig;
use tokio::sync::mpsc;
use std::collections::HashMap;
use serde_json::json;

#[tokio::test]
async fn test_counter_data() {
    let (tx, mut rx) = mpsc::channel(100);
    let (event_tx, _) = mpsc::channel(100);

    let mut properties = HashMap::new();
    properties.insert("data_type".to_string(), json!("counter"));
    properties.insert("interval_ms".to_string(), json!(100));

    let config = SourceConfig {
        id: "test-counter".to_string(),
        source_type: "mock".to_string(),
        auto_start: false,
        properties,
        bootstrap_provider: None,
    };

    let source = MockSource::new(config, tx, event_tx);
    source.start().await.unwrap();

    // Receive and validate data
    if let Some(event) = rx.recv().await {
        // Process event
    }

    source.stop().await.unwrap();
}
```

### Integration Tests

```rust
use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};

#[tokio::test]
async fn test_with_mock_source() {
    let mut core = DrasiServerCore::builder("test-core")
        .add_source(
            Source::mock("test-source")
                .with_property("data_type", json!("counter"))
                .with_property("interval_ms", json!(1000))
                .build()
        )
        .add_query(
            Query::new("test-query")
                .cypher("MATCH (c:Counter) WHERE c.value > 5 RETURN c")
                .add_source("test-source")
                .build()
        )
        .add_reaction(
            Reaction::log("test-logger")
                .add_query("test-query")
                .build()
        )
        .build();

    core.initialize().await.unwrap();
    core.start().await.unwrap();

    // Test logic here

    core.stop().await.unwrap();
}
```

## Performance Characteristics

### Generation Rate
- **Default Interval:** 5000ms (5 seconds)
- **Configurable Range:** Any positive integer (milliseconds)
- **Typical Usage:** 100ms - 10000ms depending on use case

### Data Volume
- **Per Event:** Single node insertion
- **Element Size:** Small (3-4 properties per node)
- **Memory Overhead:** Minimal (no persistent storage)

### Throughput Recommendations
- **Testing:** 100-1000ms intervals for rapid testing
- **Demos:** 1000-3000ms intervals for visible but not overwhelming data
- **Load Testing:** 10-100ms intervals for high-volume scenarios

**Note:** Very short intervals (< 100ms) may impact system performance depending on query complexity and reaction processing time.

## Implementation Details

### Data Generation Logic
- Runs as a Tokio async task
- Uses `tokio::time::interval` for precise timing
- Generates data on each interval tick
- Publishes via `SourceEventSender` channel

### Element IDs
All generated nodes have predictable element IDs based on the pattern:
- Counter: `counter_{sequence}`
- Sensor: `reading_{sensor_id}_{sequence}`
- Generic: `generic_{sequence}`

The sequence number increments with each generation cycle, starting from 1.

### Timestamps
All modes include an RFC3339 formatted timestamp showing when the data was generated:
```
2025-10-19T10:30:45.123456789Z
```

### Randomization
- **Sensor temperature:** Uses `rand::random::<f64>() * 10.0 + 20.0`
- **Sensor humidity:** Uses `rand::random::<f64>() * 20.0 + 40.0`
- **Sensor ID:** Uses `rand::random::<u32>() % 5`
- **Generic value:** Uses `rand::random::<i32>()`

## Known Limitations

1. **Insert-Only Operations:** The mock source only generates insert operations, never updates or deletes
2. **Node-Only Data:** Only generates nodes, not relationships/edges
3. **No Relationships:** Cannot generate graph relationships between nodes
4. **Fixed Schemas:** Each mode has a fixed schema that cannot be customized via configuration
5. **No Custom Properties:** Cannot add custom properties beyond the predefined schema
6. **Predictable Randomness:** Random values are not cryptographically secure (uses standard rand crate)
7. **No State Persistence:** Stops generating when source is stopped, sequence resets on restart
8. **Single Label Per Mode:** Each mode generates nodes with exactly one label

## Troubleshooting

### No Data Being Generated

**Check:**
- Source `auto_start` is set to `true` or source has been manually started
- `interval_ms` is not set to an extremely large value
- Check logs for "Starting mock source" and "Mock source started successfully" messages

### Wrong Data Type

**Check:**
- `data_type` property is spelled correctly (case-sensitive)
- Valid values are: `"counter"`, `"sensor"`, `"generic"`
- Default is `"generic"` if not specified or invalid value provided

### Query Returns No Results

**Check:**
- Node label matches the data type: `Counter`, `SensorReading`, or `Generic`
- Property names match the schema for the selected data type
- Query filter conditions are appropriate for the generated data ranges

## Related Documentation

- **Bootstrap Providers:** See `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/bootstrap/README.md` for initial data loading
- **Source API:** See `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/api/source.rs` for programmatic source construction
- **Integration Examples:** See `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/examples/basic_integration/`
- **Configuration Examples:** See `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/examples/configs/basic-mock-source/`
