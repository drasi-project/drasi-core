#!/usr/bin/env bash
set -euo pipefail

echo "Running update sequence..."

docker exec drasi-neo4j-example cypher-shell -u neo4j -p testpassword \
  "CREATE (:Person {id:'3', name:'Carrie-Anne Moss'});"

docker exec drasi-neo4j-example cypher-shell -u neo4j -p testpassword \
  "MATCH (p:Person {id:'3'}) SET p.name = 'Carrie-Anne Moss Updated';"

docker exec drasi-neo4j-example cypher-shell -u neo4j -p testpassword \
  "MATCH (p:Person {id:'3'}) DETACH DELETE p;"

echo "Updates applied. Check example output for detected changes."
