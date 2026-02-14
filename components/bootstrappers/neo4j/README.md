# Neo4j Bootstrap Provider

`drasi-bootstrap-neo4j` provides initial graph data loading from Neo4j.

## What it does

- Reads nodes from Neo4j with `elementId`, labels, and properties
- Reads relationships with start/end element IDs and properties
- Emits `SourceChange::Insert` bootstrap events in Drasi graph format

## Configuration

- `uri`
- `user`
- `password`
- `database`
- `labels` (optional filter)
- `rel_types` (optional filter)

## Licensing

Neo4j CDC and this source workflow require Neo4j Enterprise edition in production.
See: https://neo4j.com/licensing/
