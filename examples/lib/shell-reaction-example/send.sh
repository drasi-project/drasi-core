#!/bin/bash

URL="http://localhost:9000/sources/sensors/events"

PAYLOAD='{
  "operation": "insert",
  "element": {
    "type": "node",
    "id": "sensor_01",
    "labels": ["sensors"],
    "properties": {
      "id": "sensor_01",
      "temperature": 54.5,
      "location": "room-a"
    }
  }
}'

echo "Sending 200 POST requests to $URL..."

for i in {1..200}
do
   response=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$URL" \
        -H "Content-Type: application/json" \
        -d "$PAYLOAD")
   
   echo "Request $i/200 completed with HTTP status: $response"
done

echo "Done!"