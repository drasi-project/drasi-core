#!/usr/bin/env bash
set -euo pipefail

echo "Container status:"
docker ps --filter name=drasi-neo4j-example

echo
echo "Neo4j health check:"
docker exec drasi-neo4j-example cypher-shell -u neo4j -p testpassword "RETURN 'ok' AS status;"

echo
echo "CDC check:"
docker exec drasi-neo4j-example cypher-shell -u neo4j -p testpassword \
  "CALL db.cdc.current() YIELD id RETURN id LIMIT 1;"
