# Neo4j Source Plugin

`drasi-source-neo4j` streams Neo4j CDC events into Drasi.

## Requirements

- Neo4j **Enterprise Edition** (CDC is enterprise-only)
- Neo4j 5.x+
- CDC enabled for the database:

```cypher
ALTER DATABASE neo4j SET OPTION txLogEnrichment 'FULL';
```

## Licensing Requirements

This source requires Neo4j Enterprise licensing for production use.  
For local testing with Docker, you must set:

```bash
NEO4J_ACCEPT_LICENSE_AGREEMENT=yes
```

See: https://neo4j.com/licensing/

## Configuration

- `uri` (required): Bolt URI (for example `bolt://localhost:7687`)
- `user` (required): Neo4j username
- `password`: Neo4j password
- `database`: Database name (`neo4j` by default)
- `labels`: Node labels to monitor (empty means all)
- `rel_types`: Relationship types to monitor (empty means all)
- `poll_interval_ms`: CDC polling interval (default 500 ms)
- `start_cursor`:
  - `now`
  - `beginning`
  - `timestamp(<ms>)`

## Integration Test

Ignored integration tests are in `tests/integration_tests.rs` and use:

- Docker image: `neo4j:5-enterprise`
- testcontainers
- CRUD assertions (insert/update/delete) and relationship detection

Run:

```bash
cargo test -p drasi-source-neo4j -- --ignored --nocapture
```
