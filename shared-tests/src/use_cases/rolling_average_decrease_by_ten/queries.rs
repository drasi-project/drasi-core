
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

pub fn rolling_average_decrease_by_ten_metadata() -> Vec<QueryJoin> {
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

pub fn rolling_average_decrease_by_ten_query() -> &'static str  {
    "
  MATCH
    (equip:Equipment {type:'freezer'})-[:HAS_SENSOR]->(:Sensor {type:'temperature'})-[:HAS_VALUE]->(val:SensorValue)
    WITH
      val,
      elementId(equip) AS freezerId,
      drasi.getVersionByTimestamp(val, drasi.max([1696150800,val.timestamp - 1])) AS previousSensorVal
    WITH
      freezerId,
      drasi.getVersionsByTimeRange(previousSensorVal,previousSensorVal.timestamp - (6 * 60 * 60), previousSensorVal.timestamp) AS previousSensorValVersions,
      drasi.getVersionsByTimeRange(val, val.timestamp - (6 * 60 * 60), val.timestamp) AS currentSensorValVersions
    WITH
      freezerId,
      reduce ( count = 0, val IN previousSensorValVersions | count + 1 ) AS previousCountTemp,
      reduce ( total = 0.0, val IN previousSensorValVersions | total + val.value) AS previousTotalTemp,
      reduce ( count = 0, val IN currentSensorValVersions | count + 1 ) AS currentCountTemp,
      reduce ( total = 0.0, val IN currentSensorValVersions | total + val.value) AS currentTotalTemp
    WITH 
        freezerId,
        previousTotalTemp / previousCountTemp AS previousAverageTemp,
        currentTotalTemp / currentCountTemp AS currentAverageTemp
    WHERE
        currentAverageTemp < previousAverageTemp * 0.9
    RETURN
        freezerId, previousAverageTemp, currentAverageTemp"

    //   let parse_result = drasi_query_cypher::parse("
    // MATCH
    //   (equip:Equipment {type:'freezer'})-[:HAS_SENSOR]->(:Sensor {type:'temperature'})-[:HAS_VALUE]->(val:SensorValue)
    //   WITH
    //     val,
    //     elementId(equip) AS freezerId,
    //     drasi.getVersionByTimestamp(val,val.timestamp - 1) AS previousSensorVal
    //   WITH
    //     freezerId,
    //     drasi.getVersionsByTimeRange(previousSensorVal,previousSensorVal.timestamp - (6 * 60 * 60), previousSensorVal.timestamp) AS previousSensorValVersions,
    //     drasi.getVersionsByTimeRange(val, val.timestamp - (6 * 60 * 60), val.timestamp) AS currentSensorValVersions
    //   WITH
    //     freezerId,
    //     reduce ( count = 0, val IN previousSensorValVersions | count + 1 ) AS previousCountTemp,
    //     reduce ( total = 0.0, val IN previousSensorValVersions | total + val.value) AS previousTotalTemp,
    //     reduce ( count = 0, val IN currentSensorValVersions | count + 1 ) AS currentCountTemp,
    //     reduce ( total = 0.0, val IN currentSensorValVersions | total + val.value) AS currentTotalTemp
    //   WITH
    //       freezerId,
    //       previousTotalTemp / previousCountTemp AS previousAverageTemp,
    //       currentTotalTemp / currentCountTemp AS currentAverageTemp
    //   WHERE
    //       currentAverageTemp < previousAverageTemp * 0.9
    //   RETURN
    //       freezerId, previousAverageTemp, currentAverageTemp").unwrap();
}
