// Copyright 2025 The Drasi Authors.
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

/// Simple query to track all entities
pub fn all_entities_query() -> &'static str {
    "
    MATCH (e:Entity)
    RETURN e.id as id, e.name as name, e.status as status, e.value as value
    "
}

/// Query to track online devices
pub fn online_devices_query() -> &'static str {
    "
    MATCH (d:Device)
    WHERE d.status = 'online'
    RETURN d.id as id, d.name as name, d.temp as temp, d.humidity as humidity
    "
}

/// Query to track sensors with temperature
pub fn sensor_readings_query() -> &'static str {
    "
    MATCH (s:Sensor)
    WHERE s.temp IS NOT NULL
    RETURN s.id as id, s.temp as temp, s.humidity as humidity, s.battery as battery
    "
}

/// Query to track relationships
pub fn connected_devices_query() -> &'static str {
    "
    MATCH (d1:Device)-[c:CONNECTED_TO]->(d2:Device)
    RETURN d1.id as from_id, d2.id as to_id, c.strength as strength
    "
}

/// Aggregation query to count entities by status
pub fn entity_count_by_status_query() -> &'static str {
    "
    MATCH (e:Entity)
    RETURN e.status as status, count(e) as count
    "
}
