# Neo4j Bootstrap Provider

`drasi-bootstrap-neo4j` provides initial graph data loading from Neo4j.

## What it does

- Reads nodes from Neo4j with `elementId`, labels, and properties
- Reads relationships with start/end element IDs and properties
- Emits `SourceChange::Insert` bootstrap events in Drasi graph format
- Captures the CDC cursor position during bootstrap so the source can resume from exactly where the snapshot ended

## Configuration

| Property | Required | Description |
|----------|----------|-------------|
| `uri` | Yes | Bolt URI (e.g. `bolt://localhost:7687` or `bolt+s://host:7687` for TLS) |
| `user` | Yes | Neo4j username |
| `password` | Yes | Neo4j password |
| `database` | Yes | Neo4j database name |
| `labels` | No | Node labels to include (empty means all) |
| `rel_types` | No | Relationship types to include (empty means all) |

## Licensing

Neo4j CDC requires Neo4j Enterprise edition in production.
See: https://neo4j.com/licensing/
