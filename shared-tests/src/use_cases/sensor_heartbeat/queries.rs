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

pub fn not_reported_query() -> &'static str {
    "
    MATCH
        (e:Equipment)-[:HAS_SENSOR]->(s:Sensor)-[:HAS_VALUE]->(v:SensorValue)
    WITH
        e,
        s,
        max(drasi.changeDateTime(v)) AS last_ts
    WHERE
        last_ts <= (datetime.realtime() - duration( { minutes: 60 } ))        
    OR
        drasi.trueLater(last_ts <= (datetime.realtime() - duration( { minutes: 60 } )), last_ts + duration( { minutes: 60 } ))
    RETURN
        e.name AS equipment,
        s.type AS sensor,
        last_ts AS last_ts
    "
}

pub fn not_reported_query_v2() -> &'static str {
    "
    MATCH
        (e:Equipment)-[:HAS_SENSOR]->(s:Sensor)-[:HAS_VALUE]->(v:SensorValue)
    WITH
        e,
        s,
        max(drasi.changeDateTime(v)) AS last_ts
    WHERE drasi.trueNowOrLater(last_ts <= (datetime.realtime() - duration( { minutes: 60 } )), last_ts + duration( { minutes: 60 } ))
    RETURN
        e.name AS equipment,
        s.type AS sensor,
        last_ts AS last_ts
    "
}

pub fn percent_not_reported_query() -> &'static str {
    "
    MATCH
        (e:Equipment)-[:HAS_SENSOR]->(s:Sensor)-[:HAS_VALUE]->(v:SensorValue)
    WITH
        e,
        max(drasi.changeDateTime(v)) AS last_ts
    WITH
        count(e) as total,
        count(
          CASE
            WHEN last_ts <= (datetime.realtime() - duration( { minutes: 60 } )) THEN 1
            WHEN drasi.trueLater(last_ts <= (datetime.realtime() - duration( { minutes: 60 } )), last_ts + duration( { minutes: 60 } )) THEN 1
            ELSE NULL
          END
        ) as not_reporting
    RETURN        
        (not_reporting / total) * 100 AS percent_not_reporting
    "
}
