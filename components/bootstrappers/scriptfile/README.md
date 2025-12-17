# ScriptFile Bootstrap Provider

A bootstrap provider for Drasi that loads initial graph data from JSONL (JSON Lines) script files.

## Overview

The ScriptFile bootstrap provider enables loading graph data (nodes and relationships) from one or more JSONL files during query initialization. This is useful for:

- **Testing and Development**: Pre-populate queries with test data without external dependencies
- **Static Data Loading**: Bootstrap queries with reference data, lookup tables, or configuration
- **Data Migration**: Import existing graph data from file-based exports
- **Reproducible Scenarios**: Create repeatable test scenarios with known data sets
- **Multi-file Datasets**: Load large datasets split across multiple JSONL files

The provider reads JSONL files sequentially, filters data based on requested labels from queries, and sends matching nodes and relationships as bootstrap events.

## Key Features

- **Label-based Filtering**: Automatically filters nodes and relationships based on query label requirements
- **Multi-file Support**: Read from multiple JSONL files in sequence
- **Comment Support**: Allows inline documentation within script files
- **Checkpoint Support**: Label records for marking positions in the script timeline
- **Automatic Sequencing**: Maintains order across multiple files
- **Robust Error Handling**: Validates file format and record structure
- **Auto-finish Generation**: Automatically adds finish record if not present in script

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides the most flexible and readable configuration approach:

```rust
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;

// Single file
let provider = ScriptFileBootstrapProvider::builder()
    .with_file("/path/to/data.jsonl")
    .build();

// Multiple files (processed in order)
let provider = ScriptFileBootstrapProvider::builder()
    .with_file("/data/nodes.jsonl")
    .with_file("/data/relationships.jsonl")
    .with_file("/data/reference.jsonl")
    .build();

// Bulk file paths
let paths = vec![
    "/data/nodes.jsonl".to_string(),
    "/data/relations.jsonl".to_string(),
];
let provider = ScriptFileBootstrapProvider::builder()
    .with_file_paths(paths)
    .build();
```

### Configuration Struct

Use the configuration struct when loading from serialized configuration:

```rust
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;
use drasi_lib::bootstrap::ScriptFileBootstrapConfig;

let config = ScriptFileBootstrapConfig {
    file_paths: vec![
        "/path/to/data.jsonl".to_string(),
        "/path/to/more_data.jsonl".to_string(),
    ],
};

let provider = ScriptFileBootstrapProvider::new(config);
```

### Direct Paths

For simple cases with known file paths:

```rust
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;

let provider = ScriptFileBootstrapProvider::with_paths(vec![
    "/data/bootstrap.jsonl".to_string(),
]);
```

### Default Provider

Create an empty provider (no files):

```rust
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;

let provider = ScriptFileBootstrapProvider::default();
```

## Configuration Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `file_paths` | List of JSONL file paths to read in order | `Vec<String>` | Valid file system paths ending in `.jsonl` | `[]` (empty) |

**Notes:**
- Files are processed in the order they appear in the `file_paths` list
- All files must have the `.jsonl` extension
- First file must begin with a Header record
- Relative or absolute paths are supported

## Input Schema

Bootstrap script files use JSONL (JSON Lines) format - one JSON object per line. Each record has a `kind` field that identifies the record type.

### Header Record

**Required** as the first record in the first file. Provides metadata about the script.

```json
{
  "kind": "Header",
  "start_time": "2024-01-01T00:00:00+00:00",
  "description": "Initial bootstrap data"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `kind` | String | Yes | Must be `"Header"` |
| `start_time` | ISO 8601 DateTime | Yes | Script reference timestamp |
| `description` | String | No | Human-readable script description |

### Node Record

Represents a graph node/entity with properties and labels.

```json
{
  "kind": "Node",
  "id": "person-1",
  "labels": ["Person", "Employee"],
  "properties": {
    "name": "Alice Smith",
    "age": 30,
    "email": "alice@example.com"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `kind` | String | Yes | Must be `"Node"` |
| `id` | String | Yes | Unique identifier for the node |
| `labels` | Array of Strings | Yes | Node type labels (can be empty array) |
| `properties` | JSON Object | No | Node properties (defaults to null/empty) |

**Property Types:** Properties support all JSON types: strings, numbers, booleans, objects, arrays, and null.

### Relation Record

Represents a graph relationship/edge between two nodes.

```json
{
  "kind": "Relation",
  "id": "knows-1",
  "labels": ["KNOWS"],
  "start_id": "person-1",
  "start_label": "Person",
  "end_id": "person-2",
  "end_label": "Person",
  "properties": {
    "since": 2020,
    "strength": 0.85
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `kind` | String | Yes | Must be `"Relation"` |
| `id` | String | Yes | Unique identifier for the relationship |
| `labels` | Array of Strings | Yes | Relationship type labels |
| `start_id` | String | Yes | ID of the source/start node |
| `start_label` | String | No | Optional label of start node |
| `end_id` | String | Yes | ID of the target/end node |
| `end_label` | String | No | Optional label of end node |
| `properties` | JSON Object | No | Relationship properties (defaults to null/empty) |

**Direction:** Relationships are directed from `start_id` to `end_id`. In Cypher: `(start)-[relation]->(end)`

### Comment Record

Documentation and notes within the script. Automatically filtered out during processing.

```json
{
  "kind": "Comment",
  "comment": "This section contains employee data"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `kind` | String | Yes | Must be `"Comment"` |
| `comment` | String | Yes | Comment text (arbitrary length) |

### Label Record

Checkpoint marker for script navigation (currently informational only).

```json
{
  "kind": "Label",
  "offset_ns": 1000000000,
  "label": "checkpoint_1",
  "description": "After employee nodes loaded"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `kind` | String | Yes | Must be `"Label"` |
| `offset_ns` | Number (u64) | No | Nanoseconds offset from script start time |
| `label` | String | Yes | Checkpoint label/name |
| `description` | String | No | Checkpoint description |

### Finish Record

Marks the end of the script. If not present, one is automatically generated.

```json
{
  "kind": "Finish",
  "description": "Bootstrap complete"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `kind` | String | Yes | Must be `"Finish"` |
| `description` | String | No | Completion message |

## Usage Examples

### Example 1: Basic Bootstrap Script

Create a simple bootstrap file with people and relationships:

**people.jsonl:**
```jsonl
{"kind":"Header","start_time":"2024-01-01T00:00:00Z","description":"People and relationships"}
{"kind":"Node","id":"alice","labels":["Person"],"properties":{"name":"Alice","age":30}}
{"kind":"Node","id":"bob","labels":["Person"],"properties":{"name":"Bob","age":35}}
{"kind":"Relation","id":"r1","labels":["KNOWS"],"start_id":"alice","end_id":"bob","properties":{"since":2020}}
{"kind":"Finish","description":"Done"}
```

**Rust code:**
```rust
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;

let provider = ScriptFileBootstrapProvider::builder()
    .with_file("/data/people.jsonl")
    .build();
```

### Example 2: Multi-file Dataset

Split large datasets across multiple files for better organization:

**nodes.jsonl:**
```jsonl
{"kind":"Header","start_time":"2024-01-01T00:00:00Z","description":"Employee database"}
{"kind":"Comment","comment":"Employee nodes"}
{"kind":"Node","id":"e1","labels":["Employee"],"properties":{"name":"Alice","dept":"Engineering"}}
{"kind":"Node","id":"e2","labels":["Employee"],"properties":{"name":"Bob","dept":"Sales"}}
```

**relationships.jsonl:**
```jsonl
{"kind":"Comment","comment":"Reporting relationships"}
{"kind":"Relation","id":"r1","labels":["REPORTS_TO"],"start_id":"e2","end_id":"e1","properties":{}}
{"kind":"Finish","description":"All data loaded"}
```

**Rust code:**
```rust
let provider = ScriptFileBootstrapProvider::builder()
    .with_file("/data/nodes.jsonl")
    .with_file("/data/relationships.jsonl")
    .build();
```

### Example 3: Label-based Filtering

Only nodes/relationships matching query labels are loaded:

**mixed_data.jsonl:**
```jsonl
{"kind":"Header","start_time":"2024-01-01T00:00:00Z","description":"Mixed entity types"}
{"kind":"Node","id":"p1","labels":["Person"],"properties":{"name":"Alice"}}
{"kind":"Node","id":"c1","labels":["Company"],"properties":{"name":"Acme Corp"}}
{"kind":"Node","id":"p2","labels":["Person","Employee"],"properties":{"name":"Bob"}}
{"kind":"Relation","id":"r1","labels":["WORKS_AT"],"start_id":"p2","end_id":"c1","properties":{}}
{"kind":"Finish"}
```

If a query requests only `Person` nodes, both `p1` and `p2` are loaded (even though `p2` has multiple labels). The `Company` node is filtered out.

**Query example:**
```cypher
MATCH (p:Person)
WHERE p.name = 'Alice'
RETURN p
```

Only nodes with the `Person` label would be loaded from the bootstrap file.

### Example 4: Complex Properties

Properties can contain nested objects, arrays, and various data types:

```jsonl
{"kind":"Header","start_time":"2024-01-01T00:00:00Z","description":"Complex data"}
{"kind":"Node","id":"user1","labels":["User"],"properties":{
  "name":"Alice",
  "age":30,
  "active":true,
  "score":95.5,
  "roles":["admin","developer"],
  "metadata":{"created":"2023-01-01","source":"import"},
  "nullable_field":null
}}
{"kind":"Finish"}
```

All standard JSON types are supported: strings, numbers, booleans, arrays, objects, and null.

### Example 5: Programmatic Usage

Using the provider in a Drasi application:

```rust
use drasi_lib::DrasiLib;
use drasi_lib::Query;
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create bootstrap provider
    let bootstrap = ScriptFileBootstrapProvider::builder()
        .with_file("/data/initial_data.jsonl")
        .build();

    // Create Drasi instance with bootstrap provider
    let drasi = DrasiLib::builder()
        .with_bootstrap_provider("script-bootstrap", bootstrap)
        .build()
        .await?;

    // Add a query that uses the bootstrap provider
    let query = Query::cypher("my-query")
        .query("MATCH (p:Person) WHERE p.age > 25 RETURN p")
        .with_bootstrap("script-bootstrap")
        .build();

    drasi.add_query(query).await?;
    drasi.start_query("my-query").await?;

    // Bootstrap data is now loaded and query is running

    Ok(())
}
```

## Implementation Details

### Label Filtering Logic

The provider automatically filters records based on the labels requested by queries:

1. **Empty label request**: If a query doesn't specify labels, all records are included
2. **Label matching**: A record is included if ANY of its labels match ANY requested label
3. **Multiple labels**: Records can have multiple labels; any match is sufficient

Example:
- Query requests: `["Person"]`
- Node has labels: `["Person", "Employee"]`
- Result: Node is included (matched on "Person")

### Multi-file Processing

When multiple files are provided:

1. Files are read sequentially in the order specified
2. The Header record must appear in the first file only
3. Subsequent files continue from where the previous file ended
4. Sequence numbers are maintained across all files
5. First Finish record encountered terminates processing

### Sequencing

Each record is assigned a sequence number:

- Header record: sequence 0
- Subsequent records: incremented for each non-comment record
- Comment records: increment sequence but are not returned
- Sequence continues across file boundaries

### Automatic Finish Record

If no Finish record exists in the script files:

- One is automatically generated after the last record
- Description: "Auto generated at end of script."
- Once reached, iteration stops

### Error Handling

The provider validates:

- File extension must be `.jsonl`
- First record must be a Header
- Each line must be valid JSON
- Record structure must match expected schema
- Properties must be JSON objects or null

Errors are returned as `anyhow::Result` with descriptive messages.

## Thread Safety

The `ScriptFileBootstrapProvider` is safe to use across threads and can be shared using `Arc`. The `bootstrap` method is async and can handle concurrent bootstrap requests from multiple queries.

## Performance Considerations

- **File I/O**: Files are read synchronously using buffered readers
- **Memory**: Records are processed one at a time (streaming), not loaded entirely into memory
- **Filtering**: Label filtering is done in-memory during iteration
- **Large Files**: Suitable for files with millions of records due to streaming approach

## Testing

The module includes comprehensive tests for:

- Builder pattern variations
- Multi-file reading
- Label filtering logic
- Comment filtering
- Automatic finish record generation
- Error cases (invalid files, missing headers, malformed JSON)
- Sequence numbering across files

Run tests:
```bash
cargo test -p drasi-bootstrap-scriptfile
```

## Dependencies

- `drasi-lib`: Core Drasi library for bootstrap provider trait
- `drasi-core`: Core models (Element, SourceChange, etc.)
- `anyhow`: Error handling
- `async-trait`: Async trait support
- `serde`/`serde_json`: JSON serialization/deserialization
- `chrono`: DateTime handling
- `log`: Logging support

## License

Licensed under the Apache License, Version 2.0. See LICENSE file for details.
