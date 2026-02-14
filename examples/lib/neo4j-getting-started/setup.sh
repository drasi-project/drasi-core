#!/usr/bin/env bash
set -euo pipefail

echo "Starting Neo4j container..."
docker compose up -d

echo "Waiting for Neo4j to be ready (60s timeout)..."
for i in $(seq 1 60); do
  if docker exec drasi-neo4j-example cypher-shell -u neo4j -p testpassword "RETURN 1;" >/dev/null 2>&1; then
    echo "Neo4j is ready"
    break
  fi

  if [ "$i" -eq 60 ]; then
    echo "Neo4j failed to start in time"
    docker logs drasi-neo4j-example --tail 100 || true
    exit 1
  fi

  if [ $((i % 10)) -eq 0 ]; then
    echo "Still waiting... (${i}/60)"
  fi
  sleep 1
done

echo "Initializing sample graph..."
docker exec drasi-neo4j-example cypher-shell -u neo4j -p testpassword \
  "ALTER DATABASE neo4j SET OPTION txLogEnrichment 'FULL';"

docker exec drasi-neo4j-example cypher-shell -u neo4j -p testpassword \
  "MATCH (n) DETACH DELETE n; \
   CREATE (:Person {id:'1', name:'Keanu Reeves'}); \
   CREATE (:Movie {id:'2', title:'The Matrix'}); \
   MATCH (p:Person {id:'1'}), (m:Movie {id:'2'}) CREATE (p)-[:ACTED_IN {role:'Neo'}]->(m);"

echo "Setup complete"
