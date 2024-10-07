// Copyright 2024 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use drasi_core::models::{QueryJoin, QueryJoinKey};

/**
apiVersion: query.reactive-graph.io/v1
kind: ContinuousQuery
metadata:
  name: crosses_above_three_times_in_an_hour
spec:
  mode: query
  sources:
    subscriptions:
      - id: Reflex.FACILITIES
        nodes:
          - sourceLabel: Equipment
          - sourceLabel: Sensor
          - sourceLabel: SensorValue
    joins:
      - id: HAS_SENSOR
        keys:
          - label: Equipment
            property: id
          - label: Sensor
            property: equip_id
      - id: HAS_VALUE
        keys:
          - label: Sensor
            property: id
          - label: SensorValue
            property: sensor_id
  query: > … Cypher Query …
*/

pub fn crosses_above_three_times_in_an_hour_metadata() -> Vec<QueryJoin> {
    vec![
        QueryJoin {
            id: "HAS_SENSOR".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Equipment".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "Sensor".into(),
                    property: "equip_id".into(),
                },
            ],
        },
        QueryJoin {
            id: "HAS_VALUE".into(),
            keys: vec![
                QueryJoinKey {
                    label: "Sensor".into(),
                    property: "id".into(),
                },
                QueryJoinKey {
                    label: "SensorValue".into(),
                    property: "sensor_id".into(),
                },
            ],
        },
    ]
}

pub fn crosses_above_three_times_in_an_hour_query() -> &'static str {
    "
  MATCH
    (equip:Equipment {type:'freezer'})-[:HAS_SENSOR]->(:Sensor {type:'temperature'})-[:HAS_VALUE]->(val:SensorValue)
  WITH
    val,
    elementId(equip) AS freezerId,
    val.timestamp - (60 * 60) AS timeRangeStart,
    val.timestamp AS timeRangeEnd
  WITH
    freezerId,
    drasi.getVersionsByTimeRange(val, timeRangeStart, timeRangeEnd ) AS sensorValVersions
  WITH 
    freezerId,
    reduce ( count = 0, sensorValVersion IN sensorValVersions | CASE WHEN sensorValVersion.value > 32 THEN count + 1 ELSE count END) AS countTempExceededInTimeRange
  WHERE 
    countTempExceededInTimeRange >= 3
  RETURN
    freezerId, countTempExceededInTimeRange"
}

// A version of the query that includes the time ranges for the sensor values
// pub fn reflex_crosses_above_three_times_in_an_hour_query() -> ast::Query {
//   drasi_query_cypher::parse("
//   MATCH
//     (equip:Equipment {type:'freezer'})-[:HAS_SENSOR]->(:Sensor {type:'temperature'})-[:HAS_VALUE]->(val:SensorValue)
//   WITH
//     val,
//     elementId(equip) AS freezerId,
//     val.timestamp - (60 * 60) AS timeRangeStart,
//     val.timestamp AS timeRangeEnd
//   WITH
//     freezerId, timeRangeStart, timeRangeEnd,
//     drasi.getVersionsByTimeRange(val, timeRangeStart, timeRangeEnd ) AS sensorValVersions
//   WITH
//     freezerId, timeRangeStart, timeRangeEnd,
//     reduce ( count = 0, sensorValVersion IN sensorValVersions | CASE WHEN sensorValVersion.value > 32 THEN count + 1 ELSE count END) AS countTempExceededInTimeRange
//   WHERE
//     countTempExceededInTimeRange >= 3
//   RETURN
//     freezerId, timeRangeStart, timeRangeEnd, countTempExceededInTimeRange").unwrap()
// }
