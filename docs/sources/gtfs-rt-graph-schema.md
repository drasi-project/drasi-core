# GTFS-RT Graph Schema Reference

This document describes the graph model emitted by the GTFS-RT source and bootstrap provider.

## Node types

### Shared nodes

| Label | ID pattern | Key properties | Notes |
|---|---|---|---|
| `Route` | `route:{route_id}` | `route_id` | Shared across trip updates, vehicle positions, and alerts |
| `Trip` | `trip:{trip_id}` | `trip_id`, `route_id`, `direction_id`, `start_time`, `start_date`, `schedule_relationship` | Shared trip identity |
| `Vehicle` | `vehicle:{vehicle_id}` | `vehicle_id`, `label`, `license_plate`, `wheelchair_accessible` | Shared vehicle identity |
| `Stop` | `stop:{stop_id}` | `stop_id` | Shared stop identity |
| `Agency` | `agency:{agency_id}` | `agency_id` | Used by alert informed entities |

### Feed-specific nodes

| Label | ID pattern | Key properties |
|---|---|---|
| `TripUpdate` | `tu:{entity_id}` | `entity_id`, `trip_id`, `route_id`, `delay`, `timestamp` |
| `StopTimeUpdate` | `stu:{entity_id}:{stop_sequence_or_index}` | `stop_sequence`, `stop_id`, `arrival_delay`, `arrival_time`, `arrival_uncertainty`, `departure_delay`, `departure_time`, `departure_uncertainty`, `schedule_relationship`, `departure_occupancy_status` |
| `VehiclePosition` | `vp:{entity_id}` | `entity_id`, `vehicle_id`, `vehicle_label`, `trip_id`, `route_id`, `latitude`, `longitude`, `bearing`, `speed`, `current_stop_sequence`, `stop_id`, `current_status`, `timestamp`, `congestion_level`, `occupancy_status`, `occupancy_percentage` |
| `Alert` | `alert:{entity_id}` | `entity_id`, `cause`, `effect`, `severity_level`, `header_text`, `description_text`, `url`, `active_period_start`, `active_period_end` |

## Relationship types

Drasi uses relation direction `(in_node)-[:REL]->(out_node)`.

| Label | ID pattern | Direction | Meaning |
|---|---|---|---|
| `ON_ROUTE` | `rel:on_route:{trip_id}` | `Trip -> Route` | Trip is assigned to route |
| `SERVES` | `rel:serves:{vehicle_id}:{trip_id}` | `Vehicle -> Trip` | Vehicle serves trip |
| `HAS_UPDATE` | `rel:has_update:{trip_id}:{entity_id}` | `Trip -> TripUpdate` | Trip has realtime update |
| `HAS_STOP_TIME_UPDATE` | `rel:has_stu:{entity_id}:{stop_sequence_or_index}` | `TripUpdate -> StopTimeUpdate` | Trip update contains stop update |
| `AT_STOP` | `rel:at_stop:{entity_id}:{stop_sequence_or_index}` | `StopTimeUpdate -> Stop` | Stop-time update references stop |
| `HAS_POSITION` | `rel:has_position:{vehicle_id}:{entity_id}` | `Vehicle -> VehiclePosition` | Vehicle has live position |
| `CURRENT_STOP` | `rel:current_stop:{entity_id}` | `VehiclePosition -> Stop` | Position references current stop |
| `AFFECTS_ROUTE` | `rel:affects_route:{entity_id}:{selector_index}` | `Alert -> Route` | Alert affects route |
| `AFFECTS_STOP` | `rel:affects_stop:{entity_id}:{selector_index}` | `Alert -> Stop` | Alert affects stop |
| `AFFECTS_AGENCY` | `rel:affects_agency:{entity_id}:{selector_index}` | `Alert -> Agency` | Alert affects agency |

## Timestamp semantics

- Element `effective_from` is derived from feed timestamps (converted to milliseconds)
- Values are validated as millisecond timestamps before emission

## Example Cypher queries

### Live vehicle map

```cypher
MATCH (v:Vehicle)-[:HAS_POSITION]->(vp:VehiclePosition)
RETURN v.vehicle_id, v.label, vp.latitude, vp.longitude, vp.route_id, vp.trip_id, vp.timestamp
```

### Delay leaderboard

```cypher
MATCH (t:Trip)-[:HAS_UPDATE]->(tu:TripUpdate)
WHERE tu.delay > 0
RETURN t.trip_id, t.route_id, tu.delay, tu.timestamp
```

### Alerts affecting routes and stops

```cypher
MATCH (a:Alert)
OPTIONAL MATCH (a)-[:AFFECTS_ROUTE]->(r:Route)
OPTIONAL MATCH (a)-[:AFFECTS_STOP]->(s:Stop)
RETURN a.entity_id, a.severity_level, a.header_text, r.route_id, s.stop_id
```
