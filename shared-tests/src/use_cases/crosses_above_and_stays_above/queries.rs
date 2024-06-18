use drasi_query_ast::ast;
use drasi_core::models::{QueryJoin, QueryJoinKey};

/**
apiVersion: query.reactive-graph.io/v1
kind: ContinuousQuery
metadata:
  name: crosses_above_and_stays_above
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

pub fn crosses_above_and_stays_above_metadata() -> Vec<QueryJoin> {
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

pub fn crosses_above_and_stays_above_query() -> &'static str {
    "
  MATCH
    (equip:Equipment {type:'freezer'})-[:HAS_SENSOR]->(:Sensor {type:'temperature'})-[:HAS_VALUE]->(val:SensorValue)
  WITH
    val,
    elementId(equip) AS freezerId,
    val.timestamp - (15 * 60) AS timeRangeStart,
    val.timestamp AS timeRangeEnd
  WITH
    freezerId,
    drasi.getVersionsByTimeRange(val, timeRangeStart, timeRangeEnd ) AS sensorValVersions
  WITH 
    freezerId,
    reduce ( minTemp = 10000.0, sensorValVersion IN sensorValVersions | CASE WHEN sensorValVersion.value < minTemp THEN sensorValVersion.value ELSE minTemp END) AS minTempInTimeRange
  WHERE 
    minTempInTimeRange > 32.0
  RETURN
    freezerId, minTempInTimeRange"
}

// A version of the query that includes the time ranges for the sensor values
// pub fn crosses_above_and_stays_above_query() -> ast::Query {
//   drasi_query_cypher::parse("
//   MATCH
//     (equip:Equipment {type:'freezer'})-[:HAS_SENSOR]->(:Sensor {type:'temperature'})-[:HAS_VALUE]->(val:SensorValue)
//   WITH
//     val,
//     elementId(equip) AS freezerId,
//     val.timestamp - (15 * 60) AS timeRangeStart,
//     val.timestamp AS timeRangeEnd
//   WITH
//     freezerId, timeRangeStart, timeRangeEnd,
//     drasi.getVersionsByTimeRange(val, timeRangeStart, timeRangeEnd ) AS sensorValVersions
//   WITH
//     freezerId, timeRangeStart, timeRangeEnd,
//     reduce ( minTemp = 10000.0, sensorValVersion IN sensorValVersions | CASE WHEN sensorValVersion.value < minTemp THEN sensorValVersion.value ELSE minTemp END) AS minTempInTimeRange
//   WHERE
//     minTempInTimeRange > 32.0
//   RETURN
//     freezerId, timeRangeStart, timeRangeEnd, minTempInTimeRange").unwrap()
// }
