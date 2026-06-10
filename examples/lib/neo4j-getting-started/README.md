# Neo4j Getting Started Example

This example runs Drasi with the Neo4j CDC source.

## Prerequisites

- Docker and Docker Compose
- Rust toolchain
- Neo4j Enterprise license acceptance for local testing

## Quick Start

```bash
./quickstart.sh
```

## Helper Scripts

- `setup.sh` - Starts Neo4j, waits up to 60s, initializes sample data
- `quickstart.sh` - Runs setup then starts the example
- `diagnose.sh` - Verifies container and CDC health
- `test-updates.sh` - Sends create/update/delete mutations

## Verify It Works

1. Run `./quickstart.sh`
2. In another terminal run `./test-updates.sh`
3. Observe Drasi log reaction output with detected changes

## Troubleshooting

- If startup fails: run `docker logs drasi-neo4j-example --tail 200`
- If CDC is unavailable: run `./diagnose.sh`
- Ensure `NEO4J_ACCEPT_LICENSE_AGREEMENT=yes` is set in `docker-compose.yml`
- Ensure `setup.sh` has run `ALTER DATABASE neo4j SET OPTION txLogEnrichment 'FULL'`
