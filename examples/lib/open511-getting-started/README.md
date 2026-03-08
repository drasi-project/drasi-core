# Open511 Getting Started (DrasiLib)

This example shows how to consume Open511 road event feeds (such as DriveBC) with Drasi using:
- `drasi-source-open511`
- `drasi-bootstrap-open511`
- a Cypher query that tracks active road events

## Prerequisites

- Rust toolchain (same as repository)
- Network access to an Open511 endpoint (default: `https://api.open511.gov.bc.ca`)

## Quick Start

```bash
cd examples/lib/open511-getting-started
./quickstart.sh
```

The example starts Drasi, bootstraps current active events, then polls every minute for updates.

## Scripts

- `setup.sh` - validates endpoint connectivity before running
- `quickstart.sh` - setup + `cargo run`
- `diagnose.sh` - fetches sample data and prints a simple health summary
- `test-updates.sh` - polls the API and prints when count/timestamp changes are observed

You can override the endpoint in scripts with:

```bash
OPEN511_API_URL="https://api.open511.gov.bc.ca" ./diagnose.sh
```

## Query Behavior

The example query:

```cypher
MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
WHERE e.status = 'ACTIVE' AND e.severity = 'MAJOR'
RETURN e.id, e.headline, e.severity, r.name
```

Results are logged through `drasi-reaction-log` as records are added, updated, or removed.
