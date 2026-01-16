# Platform Bootstrap Provider

A bootstrap provider for Drasi that fetches initial data from a remote Drasi Query API service. This enables federated Drasi deployments where one Drasi instance can bootstrap queries with data from another Drasi instance.

## Overview

The Platform Bootstrap Provider connects to a remote Drasi Query API service to retrieve initial graph data (nodes and relationships) for continuous query bootstrap operations. It establishes an HTTP connection, subscribes to query results, and streams elements in JSON-NL (JSON Lines) format.

### Key Capabilities

- **Remote Data Bootstrap**: Fetches initial graph data from remote Drasi Query API services
- **Streaming JSON-NL Processing**: Efficiently processes large datasets via streaming HTTP responses
- **Label-Based Filtering**: Filters nodes and relationships by requested labels to optimize data transfer
- **Configurable Timeouts**: Supports custom HTTP timeout settings for long-running bootstrap operations
- **Automatic Type Detection**: Distinguishes between nodes and relationships based on element structure

### Use Cases

1. **Federated Drasi Deployments**: Bootstrap queries in one Drasi instance with data from another
2. **Data Aggregation**: Combine data from multiple remote Drasi sources into a single query
3. **Cross-Environment Synchronization**: Bootstrap development/staging environments from production
4. **Distributed Graph Processing**: Connect multiple Drasi instances in a processing pipeline

## Configuration

The Platform Bootstrap Provider supports three configuration approaches:

### 1. Builder Pattern (Recommended)

```rust
use drasi_bootstrap_platform::PlatformBootstrapProvider;

let provider = PlatformBootstrapProvider::builder()
    .with_query_api_url("http://remote-drasi:8080")
    .with_timeout_seconds(600)
    .build()?;
```

### 2. Configuration Struct

```rust
use drasi_bootstrap_platform::PlatformBootstrapProvider;
use drasi_lib::bootstrap::PlatformBootstrapConfig;

let config = PlatformBootstrapConfig {
    query_api_url: Some("http://remote-drasi:8080".to_string()),
    timeout_seconds: 600,
};

let provider = PlatformBootstrapProvider::new(config)?;
```

### 3. Direct Constructor

```rust
use drasi_bootstrap_platform::PlatformBootstrapProvider;

let provider = PlatformBootstrapProvider::with_url(
    "http://remote-drasi:8080",
    600  // timeout in seconds
)?;
```

## Configuration Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `query_api_url` | Base URL of the remote Drasi Query API service | `String` | Valid HTTP/HTTPS URL (e.g., "http://drasi:8080") | **Required** |
| `timeout_seconds` | HTTP request timeout in seconds | `u64` | Positive integer (recommended: 300-900 for large datasets) | `300` |

### Configuration Notes

- **query_api_url**: Must be a complete URL including protocol and port. The provider will append `/subscription` to this base URL for subscription requests.
- **timeout_seconds**: Should be set based on expected dataset size. Large bootstraps may require 600+ seconds. The timeout applies to the entire HTTP request including streaming.

## Input Schema

The Platform Bootstrap Provider expects JSON-NL (newline-delimited JSON) responses from the remote Query API's `/subscription` endpoint. Each line contains a single element in the following format:

### Bootstrap Element Schema

```json
{
  "id": "unique-element-id",
  "labels": ["Label1", "Label2"],
  "properties": {
    "propertyName": "value",
    "anotherProperty": 42
  },
  "startId": "optional-start-node-id",
  "endId": "optional-end-node-id"
}
```

### Field Descriptions

- **id** (required): Unique identifier for the element within the source
- **labels** (required): Array of label strings (e.g., `["Person", "Employee"]`)
- **properties** (required): Object containing element properties (supports string, number, boolean, null values)
- **startId** (optional): For relationships only - ID of the start/source node
- **endId** (optional): For relationships only - ID of the end/target node

### Element Type Detection

- **Node**: Elements without `startId` and `endId` fields are treated as nodes
- **Relationship**: Elements with both `startId` and `endId` are treated as relationships

### Example: Node Element

```json
{
  "id": "person-1",
  "labels": ["Person", "Employee"],
  "properties": {
    "name": "Alice",
    "age": 30,
    "department": "Engineering"
  }
}
```

### Example: Relationship Element

```json
{
  "id": "rel-1",
  "labels": ["WORKS_FOR"],
  "properties": {
    "since": "2020-01-15",
    "role": "Senior Engineer"
  },
  "startId": "person-1",
  "endId": "company-1"
}
```

### HTTP Subscription Request

The provider sends POST requests to `{query_api_url}/subscription` with the following payload:

```json
{
  "queryId": "my-query-id",
  "queryNodeId": "server-instance-id",
  "nodeLabels": ["Person", "Company"],
  "relLabels": ["WORKS_FOR", "MANAGES"]
}
```

## Usage Examples

### Basic Bootstrap Setup

```rust
use drasi_bootstrap_platform::PlatformBootstrapProvider;
use drasi_lib::DrasiLibBuilder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create platform bootstrap provider
    let bootstrap_provider = PlatformBootstrapProvider::builder()
        .with_query_api_url("http://source-drasi:8080")
        .with_timeout_seconds(600)
        .build()?;

    // Configure Drasi with platform bootstrap
    let drasi = DrasiLibBuilder::new()
        .with_id("federated-instance")
        .with_bootstrap_provider(bootstrap_provider)
        .build()
        .await?;

    // Add sources, queries, etc.
    // ...

    Ok(())
}
```

### Federated Query Example

```rust
use drasi_bootstrap_platform::PlatformBootstrapProvider;
use drasi_lib::{DrasiLibBuilder, Query};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Bootstrap from remote production Drasi instance
    let prod_bootstrap = PlatformBootstrapProvider::with_url(
        "http://production-drasi:8080",
        900  // 15 minutes for large dataset
    )?;

    let drasi = DrasiLibBuilder::new()
        .with_id("staging-instance")
        .with_bootstrap_provider(prod_bootstrap)
        .build()
        .await?;

    // Create query that will bootstrap from production
    let query = Query::cypher("active-users")
        .query("MATCH (u:User) WHERE u.active = true RETURN u")
        .from_source("production")
        .build();

    drasi.add_query(query).await?;

    Ok(())
}
```

### Multiple Remote Sources

```rust
use drasi_bootstrap_platform::PlatformBootstrapProvider;
use drasi_lib::DrasiLibBuilder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create providers for different remote sources
    let users_bootstrap = PlatformBootstrapProvider::with_url(
        "http://users-drasi:8080",
        300
    )?;

    let products_bootstrap = PlatformBootstrapProvider::with_url(
        "http://products-drasi:8080",
        300
    )?;

    // Configure Drasi with multiple bootstrap providers
    // Note: You would need to configure separate sources with different
    // bootstrap providers for each remote system
    let drasi = DrasiLibBuilder::new()
        .with_id("aggregation-instance")
        .with_bootstrap_provider(users_bootstrap)
        .build()
        .await?;

    Ok(())
}
```

### Error Handling

```rust
use drasi_bootstrap_platform::PlatformBootstrapProvider;

fn create_provider() -> anyhow::Result<PlatformBootstrapProvider> {
    let provider = PlatformBootstrapProvider::builder()
        .with_query_api_url("http://remote-drasi:8080")
        .with_timeout_seconds(600)
        .build()
        .map_err(|e| {
            eprintln!("Failed to create bootstrap provider: {}", e);
            e
        })?;

    Ok(provider)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match create_provider() {
        Ok(provider) => {
            println!("Provider created successfully");
            // Use provider...
        }
        Err(e) => {
            eprintln!("Initialization failed: {}", e);
            return Err(e);
        }
    }

    Ok(())
}
```

## Implementation Details

### Bootstrap Flow

1. **Subscription Request**: Provider sends POST request to `{query_api_url}/subscription` with query details and requested labels
2. **Streaming Response**: Remote service responds with JSON-NL stream of graph elements
3. **Label Filtering**: Provider filters elements based on requested node/relationship labels
4. **Element Transformation**: Converts platform SDK format to Drasi internal `Element` format
5. **Channel Distribution**: Sends transformed elements via async channels to query engine

### Label Filtering Behavior

- **Empty label list**: Matches all elements (no filtering)
- **Non-empty label list**: Element must have at least one matching label
- **Node vs Relationship**: Filtering is applied separately for nodes and relationships
- **Filtered elements**: Logged and counted but not sent to query engine

### Performance Characteristics

- **Streaming Processing**: Elements processed incrementally as received (no full buffering)
- **Progress Logging**: Logs progress every 1000 elements during large bootstraps
- **Memory Efficiency**: Line-by-line parsing minimizes memory footprint
- **Timeout Handling**: Single timeout applies to entire HTTP request/stream

### Error Handling

The provider handles various error scenarios:

- **Connection Failures**: Returns error if unable to connect to remote Query API
- **Invalid URLs**: Validates URL format during construction
- **HTTP Errors**: Returns detailed error with status code and response body
- **JSON Parsing Errors**: Logs warnings and continues processing remaining elements
- **Channel Errors**: Returns error if unable to send elements to query engine
- **Timeout Errors**: Returns error if bootstrap exceeds configured timeout

### Logging

The provider uses structured logging with the following levels:

- **INFO**: Bootstrap start/completion, element counts
- **DEBUG**: Subscription requests, streaming progress, filtering statistics
- **WARN**: JSON parsing failures for individual elements

## Dependencies

- **drasi-lib**: Core Drasi library for bootstrap provider trait
- **drasi-core**: Graph data models (Element, ElementReference, SourceChange)
- **reqwest**: HTTP client with JSON and streaming support
- **serde/serde_json**: JSON serialization and deserialization
- **tokio**: Async runtime for HTTP requests and channel operations
- **futures**: Stream processing utilities
- **anyhow**: Error handling and context
- **log**: Logging framework
- **chrono**: Timestamp generation

## Testing

The provider includes comprehensive unit tests:

```bash
# Run all tests
cargo test -p drasi-bootstrap-platform

# Run specific test
cargo test -p drasi-bootstrap-platform test_platform_bootstrap_builder_with_valid_url

# Run with debug logging
RUST_LOG=debug cargo test -p drasi-bootstrap-platform -- --nocapture
```

### Test Coverage

- Builder pattern construction and validation
- Configuration struct validation
- URL validation (valid, invalid, missing)
- Label filtering logic (empty, matching, non-matching)
- Element transformation (nodes, relationships, properties)
- Error scenarios (missing URL, invalid URL)

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
