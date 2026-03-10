# Open511 Source — Graph Schema Reference

This document describes the labeled property graph produced by the Open511 source.
Use it as a reference when writing Cypher queries against Open511 road event data.

## Graph Overview

```
┌─────────────┐     AFFECTS_ROAD      ┌──────────┐
│  RoadEvent  │──────────────────────▶│   Road   │
│             │   (one per road)      │          │
└─────────────┘                       └──────────┘
       │
       │  IN_AREA
       │  (one per area)
       ▼
┌──────────┐
│   Area   │
│ (shared) │
└──────────┘
```

Each Open511 event produces **one `RoadEvent` node**, plus one `Road` node and
`AFFECTS_ROAD` relationship per affected road, and one `Area` node (deduplicated)
and `IN_AREA` relationship per affected area.

---

## Nodes

### RoadEvent

**Label:** `RoadEvent`

| Property | Type | Required | Description |
|---|---|---|---|
| `id` | String | ✅ | Event identifier (e.g. `drivebc.ca/DBC-53013`) |
| `status` | String | ✅ | Event status: `ACTIVE`, `ARCHIVED` |
| `url` | String | | Link to the event on the jurisdiction's website |
| `jurisdiction_url` | String | | Jurisdiction identifier URL |
| `headline` | String | | Short headline (e.g. `"INCIDENT"`, `"CONSTRUCTION"`) |
| `description` | String | | Full text description of the event |
| `event_type` | String | | Type: `INCIDENT`, `CONSTRUCTION`, `SPECIAL_EVENT`, `WEATHER_CONDITION` |
| `event_subtypes` | List | | List of subtype strings (e.g. `["VEHICLE_ACCIDENT", "HAZMAT"]`) |
| `severity` | String | | Severity level: `MINOR`, `MODERATE`, `MAJOR`, `UNKNOWN` |
| `created` | String | | ISO-8601 timestamp when the event was first reported |
| `updated` | String | | ISO-8601 timestamp of the last update |
| `ivr_message` | String | | Automated phone system message text |
| `schedule` | Object | | Raw schedule object (e.g. `{intervals: ["2023-06-05T23:08/"]}`) |
| `geography` | Object | | Raw GeoJSON geometry object (e.g. `{type: "Point", coordinates: [-122.37, 52.21]}`) |
| `road_count` | Integer | ✅ | Number of roads affected by this event |
| `area_count` | Integer | ✅ | Number of areas associated with this event |

### Road

**Label:** `Road`

| Property | Type | Required | Description |
|---|---|---|---|
| `id` | String | ✅ | Generated: `{event_id}::road::{index}` |
| `event_id` | String | ✅ | Parent event ID |
| `name` | String | | Road name (e.g. `"Highway 1"`, `"Trans-Canada Highway"`) |
| `from` | String | | Start location description |
| `to` | String | | End location description |
| `direction` | String | | `BOTH`, `NORTHBOUND`, `SOUTHBOUND`, `EASTBOUND`, `WESTBOUND` |
| `state` | String | | `CLOSED`, `SOME_LANES_CLOSED`, `SINGLE_LANE_ALTERNATING`, `OPEN` |
| `delay` | String | | Estimated traffic delay |

### Area

**Label:** `Area`

Areas are **shared across events** — if multiple events reference the same
area ID, only one `Area` node is created. Each event gets its own `IN_AREA`
relationship to the shared node.

| Property | Type | Required | Description |
|---|---|---|---|
| `id` | String | ✅ | Area ID from API, or generated `{event_id}::area::{index}` if missing |
| `name` | String | | Area display name (e.g. `"Rocky Mountain District"`) |
| `url` | String | | Area URL |

---

## Relationships

### AFFECTS_ROAD

**Direction:** `(RoadEvent)-[:AFFECTS_ROAD]->(Road)`

One relationship per road listed in the event.

| Property | Type | Description |
|---|---|---|
| `event_id` | String | The parent event ID |

### IN_AREA

**Direction:** `(RoadEvent)-[:IN_AREA]->(Area)`

One relationship per area listed in the event.

*No additional properties.*

---

## ID Generation

| Element | ID Format | Example |
|---|---|---|
| RoadEvent | Raw event ID from API | `drivebc.ca/DBC-53013` |
| Road | `{event_id}::road::{index}` | `drivebc.ca/DBC-53013::road::0` |
| Area (with ID) | Raw area ID from API | `drivebc.ca/3` |
| Area (no ID) | `{event_id}::area::{index}` | `drivebc.ca/DBC-53013::area::0` |
| AFFECTS_ROAD | `{event_id}::affects_road::{index}` | `drivebc.ca/DBC-53013::affects_road::0` |
| IN_AREA | `{event_id}::in_area::{area_id}` | `drivebc.ca/DBC-53013::in_area::drivebc.ca/3` |

---

## Example Queries

### All active events with road details

```cypher
MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
WHERE e.status = 'ACTIVE'
RETURN e.id, e.headline, e.severity,
       r.name, r.direction, r.state
```

### Major incidents on a specific highway

```cypher
MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
WHERE e.status = 'ACTIVE'
  AND e.severity = 'MAJOR'
  AND r.name CONTAINS 'Highway 1'
RETURN e.id, e.headline, e.description,
       r.from, r.to, r.state
```

### Events in a specific area

```cypher
MATCH (e:RoadEvent)-[:IN_AREA]->(a:Area)
WHERE a.name = 'Rocky Mountain District'
RETURN e.id, e.headline, e.severity, e.event_type
```

### Road closures with location context

```cypher
MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
WHERE r.state = 'CLOSED'
RETURN e.id, e.headline, e.severity,
       r.name, r.from, r.to, r.direction,
       e.updated
```

### Events with geographic data

```cypher
MATCH (e:RoadEvent)
WHERE e.geography IS NOT NULL
RETURN e.id, e.headline, e.geography
```

### Count of events by severity

```cypher
MATCH (e:RoadEvent)
WHERE e.status = 'ACTIVE'
RETURN e.severity, count(e) AS event_count
```

---

## Data Lifecycle

- **Bootstrap** loads all currently matching events from the API as `INSERT` changes.
- **Incremental polls** (every N seconds) detect new and updated events via `updated>timestamp`.
- **Full-sweep polls** (every Mth cycle) compare the full API snapshot against local state to detect removed events, emitting `DELETE` changes.
- `Road` nodes are **replaced** on each event update (old roads deleted, new roads inserted).
- `Area` nodes are **shared and deduplicated** — created once and reused across events.

## Notes

- Optional properties are omitted entirely (not `null`) when the API does not provide them.
- `event_subtypes`, `schedule`, and `geography` are stored as nested objects/lists, preserving their original JSON structure.
- `geography` contains the raw GeoJSON geometry (`type` + `coordinates`).
- Timestamps (`created`, `updated`) are stored as-is from the API (ISO-8601 with timezone).
