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
  name: rolling_average_decreases
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
      - id: EQUIPMENT_SENSOR_VALUE
        keys:
          - label: Equipment
            property: id
          - label: SensorValue
            property: equip_id
      - id: SENSOR_VALUE
        keys:
          - label: Sensor
            property: id
          - label: SensorValue
            property: sensor_id
  query: > … Cypher Query …
 */
pub fn exceeds_one_standard_deviation_metadata() -> Vec<QueryJoin> {
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

pub fn exceeds_one_standard_deviation_query() -> &'static str {
    "
    MATCH
        (equip:Equipment {type:'freezer'})-[:HAS_SENSOR]->(:Sensor {type:'temperature'})-[:HAS_VALUE]->(val:SensorValue)
    WITH
        val,
        elementId(equip) AS freezerId,
        drasi.listMax([1696150800, val.timestamp - (7 * 24 * 60 * 60)]) AS timeRangeStart
    WITH
        freezerId,
        val,
        drasi.getVersionsByTimeRange(val, timeRangeStart, val.timestamp) AS sensorValVersions
    WITH
        freezerId,
        val,
        [x IN sensorValVersions | x.value] AS sensorValVersionsList,
        reduce (count = 0, v in sensorValVersions | count + 1 ) AS countTemp,
        reduce (total = 0.0, v in sensorValVersions | total + v.value) AS totalTemp
    WITH
        freezerId,
        val.value AS currentTemp,
        totalTemp / countTemp AS averageTemp,
        drasi.stdevp(sensorValVersionsList) AS stdevTemp
    WHERE
        (currentTemp < (averageTemp - stdevTemp)) OR (currentTemp > (averageTemp + stdevTemp))
    RETURN
        freezerId, currentTemp, averageTemp, stdevTemp"
}
