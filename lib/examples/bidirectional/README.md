# Bidirectional Integration Example

Complete demonstration of in-process data integration using ApplicationSource and ApplicationReaction.

## What This Shows

- **ApplicationSource**: Sending data changes into DrasiLib from your application
- **ApplicationReaction**: Receiving query results back into your application
- **Async patterns**: Spawning tasks, processing streams
- **Query semantics**: Understanding Adding/Updating/Removing result types

## Running

```bash
cargo run
```

## What Happens

The example simulates a temperature monitoring system:

1. Creates a query that detects sensors with temperature > 75
2. Sends sensor readings with varying temperatures
3. Shows how the query emits:
   - **Adding** when a sensor crosses above threshold
   - **Updating** when an already-hot sensor changes
   - **Removing** when a sensor drops below threshold or is deleted

## Key Code Patterns

### Getting Handles

```rust
// Get source handle to send data
let source = core.source_handle("app-source")?;

// Get reaction handle to receive results
let reaction = core.reaction_handle("app-reaction")?;
```

### Sending Changes

```rust
// Insert
source.send_node_insert(
    "sensor-1",
    vec!["Sensor"],
    PropertyMapBuilder::new()
        .with_string("id", "sensor-1")
        .with_integer("temperature", 70)
        .build()
).await?;

// Update
source.send_node_update(
    "sensor-1",
    vec!["Sensor"],
    PropertyMapBuilder::new()
        .with_string("id", "sensor-1")
        .with_integer("temperature", 80)
        .build()
).await?;

// Delete
source.send_delete("sensor-1", vec!["Sensor"]).await?;
```

### Receiving Results

```rust
let mut stream = reaction.as_stream().await.unwrap();

while let Some(result) = stream.next().await {
    match result.results[0] {
        Adding { after } => {
            // Sensor crossed threshold
        }
        Updating { before, after } => {
            // Hot sensor changed
        }
        Removing { before } => {
            // Sensor dropped below threshold
        }
    }
}
```

## Expected Output

```
1. Sending sensor-1 with temp=70 (below threshold)
   -> No query result (doesn't match WHERE clause)

2. Sending sensor-2 with temp=80 (above threshold)
   -> Received: Adding { after: {"id": "sensor-2", "temperature": 80} }

3. Updating sensor-2 temp to 85
   -> Received: Updating { before: {..., "temperature": 80}, after: {..., "temperature": 85} }

4. Updating sensor-1 temp to 78 (crosses threshold)
   -> Received: Adding { after: {"id": "sensor-1", "temperature": 78} }

5. Updating sensor-2 temp to 70 (drops below threshold)
   -> Received: Removing { before: {"id": "sensor-2", "temperature": 85} }

6. Deleting sensor-1
   -> Received: Removing { before: {"id": "sensor-1", "temperature": 78} }
```

## Use This Pattern When

- Embedding Drasi in your application
- Need tight integration with business logic
- Want to avoid external message brokers
- Need in-process change detection

## See Also

- [Application Source documentation](../../README.md#application-source)
- [Application Reaction documentation](../../README.md#application-reaction)
- [Understanding Query Results](../../README.md#understanding-query-results)
