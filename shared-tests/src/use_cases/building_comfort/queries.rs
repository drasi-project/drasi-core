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

pub fn room_comfort_level_calc_query() -> &'static str {
    "
  MATCH
    (r:Room)
  RETURN
    elementId(r) AS RoomId,
    floor(
      50 + (r.temp - 72) + (r.humidity - 42) +
      CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END
    ) AS ComfortLevel
    "
}

pub fn floor_comfort_level_calc_query() -> &'static str {
    "
  MATCH
    (r:Room)-[:PART_OF]->(f:Floor)
    WITH
      f,
      floor(50 + (r.temp - 72) + (r.humidity - 42) + 
        CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END 
      ) AS RoomComfortLevel 
  RETURN 
    elementId(f) AS FloorId, 
    avg(RoomComfortLevel) AS ComfortLevel
    "
}

pub fn building_comfort_level_calc_query() -> &'static str {
    "
    MATCH
      (r:Room)-[:PART_OF]->(f:Floor)-[:PART_OF]->(b:Building)
    WITH
      b,
      f,
      floor(
        50 + (r.temp - 72) + (r.humidity - 42) +
        CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END
      ) AS RoomComfortLevel
    WITH
      b,
      elementId(f) AS FloorId,
      avg(RoomComfortLevel) AS FloorComfortLevel
    RETURN
      elementId(b) AS BuildingId,
      avg(FloorComfortLevel) AS ComfortLevel
    "
}

pub fn room_comfort_level_alert_query() -> &'static str {
    "
    MATCH
      (r:Room)
    WITH
      elementId(r) AS RoomId,
      floor(
        50 + (r.temp - 72) + (r.humidity - 42) +
        CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END
      ) AS ComfortLevel
    WHERE
      ComfortLevel < 40 OR ComfortLevel > 50
    RETURN
      RoomId,
      ComfortLevel
    "
}

pub fn floor_comfort_level_alert_query() -> &'static str {
    "
    MATCH
      (r:Room)-[:PART_OF]->(f:Floor)
    WITH
      f,
      floor(
        50 + (r.temp - 72) + (r.humidity - 42) +
        CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END
      ) AS RoomComfortLevel
    WITH
      elementId(f) AS FloorId,
      avg(RoomComfortLevel) AS ComfortLevel
    WHERE
      ComfortLevel < 40 OR ComfortLevel > 50
    RETURN
      FloorId,
      ComfortLevel
    "
}

pub fn building_comfort_level_alert_query() -> &'static str {
    "
    MATCH
      (r:Room)-[:PART_OF]->(f:Floor)-[:PART_OF]->(b:Building)
    WITH
      b,
      f,
      floor(
        50 + (r.temp - 72) + (r.humidity - 42) +
        CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END
      ) AS RoomComfortLevel
    WITH
      b,
      elementId(f) AS FloorId,
      avg(RoomComfortLevel) AS FloorComfortLevel
    WITH
      elementId(b) AS BuildingId,
      avg(FloorComfortLevel) AS ComfortLevel
    WHERE
      ComfortLevel < 40 OR ComfortLevel > 50
    RETURN
      BuildingId,
      ComfortLevel
    "
}

pub fn ui_query() -> &'static str {
    "
  MATCH
    (r:Room)-[:PART_OF]->(f:Floor)-[:PART_OF]->(b:Building)
  WITH
    r,
    f,
    b,
    floor(
      50 + (r.temp - 72) + (r.humidity - 42) +
      CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END
    ) AS RoomComfortLevel
  RETURN
    elementId(r) AS RoomId,
    r.name AS RoomName,
    elementId(f) AS FloorId,
    f.name AS FloorName,
    elementId(b) AS BuildingId,
    b.name AS BuildingName,
    r.temp AS Temperature,
    r.humidity AS Humidity,
    r.co2 AS CO2,
    RoomComfortLevel AS ComfortLevel
    "
}
