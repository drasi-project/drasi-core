# drasi-source-open511

A Drasi source that continuously monitors [Open511](http://www.open511.org/) road event
APIs and turns them into a live graph of road events, affected roads, and geographic areas.

Open511 is an open standard for sharing road event data adopted by transportation
agencies across North America. This source polls any compliant endpoint—DriveBC,
511 Alberta, Québec 511, and others—and emits graph changes as events are created,
updated, or cleared.

## How It Works

```
Open511 REST API ──(poll)──▶ Open511Source ──▶ Graph changes ──▶ Continuous Queries
                                  │
                       ┌──────────┼──────────┐
                       ▼          ▼          ▼
                   RoadEvent    Road       Area
                     nodes      nodes      nodes
                       │          ▲          ▲
                       ├──────────┘          │
                       │   AFFECTS_ROAD      │
                       └─────────────────────┘
                             IN_AREA
```

The source uses a hybrid polling strategy:

- **Incremental polls** — most cycles query only events updated since the last poll
  (`updated=>timestamp`), keeping API traffic minimal.
- **Full sweeps** — every Nth cycle fetches all events to detect removals that
  incremental polling alone cannot observe.

See [GRAPH_SCHEMA.md](GRAPH_SCHEMA.md) for the complete graph model reference.

## Quick Start

```rust
use drasi_source_open511::Open511Source;
use drasi_bootstrap_open511::Open511BootstrapProvider;

let bootstrap = Open511BootstrapProvider::builder()
    .with_base_url("https://api.open511.gov.bc.ca")
    .with_status_filter("ACTIVE")
    .build()?;

let source = Open511Source::builder("road-events")
    .with_base_url("https://api.open511.gov.bc.ca")
    .with_poll_interval_secs(60)
    .with_full_sweep_interval(10)
    .with_status_filter("ACTIVE")
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

## Configuration

| Parameter | Default | Description |
|---|---|---|
| `base_url` | *(required)* | Open511 API base URL |
| `poll_interval_secs` | `60` | Seconds between poll cycles |
| `full_sweep_interval` | `10` | Run a full sweep every N polls |
| `request_timeout_secs` | `15` | HTTP request timeout |
| `page_size` | `500` | Results per API page (max 500) |
| `status_filter` | `ACTIVE` | Filter by event status |
| `severity_filter` | — | Filter by severity (`MINOR`, `MODERATE`, `MAJOR`) |
| `event_type_filter` | — | Filter by type (`INCIDENT`, `CONSTRUCTION`, etc.) |
| `area_id_filter` | — | Filter by area ID |
| `road_name_filter` | — | Filter by road name |
| `jurisdiction_filter` | — | Filter by jurisdiction URL |
| `bbox_filter` | — | Bounding box `xmin,ymin,xmax,ymax` |
| `initial_cursor_behavior` | `StartFromBeginning` | How to initialize when no state exists |

### Initial Cursor Behavior

| Mode | Behavior |
|---|---|
| `StartFromBeginning` | First poll is a full sweep; all current events are emitted as inserts |
| `StartFromNow` | Snapshot current events silently; only future changes trigger reactions |
| `StartFromTimestamp` | Begin incremental polling from a fixed point in time |

## Known Open511 Endpoints

| Agency | Region | Base URL |
|---|---|---|
| DriveBC | British Columbia | `https://api.open511.gov.bc.ca` |
| 511 Alberta | Alberta | `https://511.alberta.ca/api` |
| Québec 511 | Québec | `https://www.quebec511.info/api` |

> Check each agency's documentation for supported filters and rate limits.

## Example Use Cases

### 1. Alert on highway closures

Notify a team channel whenever a highway is fully closed:

```rust
let query = Query::cypher("highway-closures")
    .query(r#"
        MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
        WHERE e.status = 'ACTIVE'
          AND r.state = 'CLOSED'
        RETURN e.id AS event_id,
               e.headline AS headline,
               e.severity AS severity,
               r.name AS road,
               r.from AS from_location,
               r.to AS to_location,
               r.direction AS direction
    "#)
    .from_source("road-events")
    .auto_start(true)
    .enable_bootstrap(true)
    .build();
```

When a road's `state` changes to `CLOSED`, the query emits an insert.
When the closure is lifted (state changes to `OPEN` or the event is archived),
it emits a delete. Pair with a webhook reaction to push alerts to Slack or Teams.

### 2. Track major incidents in a geographic area

Monitor only major incidents in the Rocky Mountain District:

```rust
let source = Open511Source::builder("rockies-incidents")
    .with_base_url("https://api.open511.gov.bc.ca")
    .with_status_filter("ACTIVE")
    .with_severity_filter("MAJOR")
    .with_area_id_filter("drivebc.ca/3")  // Rocky Mountain District
    .with_poll_interval_secs(30)           // check more frequently
    .with_bootstrap_provider(bootstrap)
    .build()?;

let query = Query::cypher("rockies-major")
    .query(r#"
        MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
        WHERE e.event_type = 'INCIDENT'
        RETURN e.id AS event_id,
               e.description AS description,
               e.severity AS severity,
               r.name AS road,
               r.state AS road_state
    "#)
    .from_source("rockies-incidents")
    .auto_start(true)
    .enable_bootstrap(true)
    .build();
```

### 3. Construction activity dashboard

Feed a dashboard with active construction events and the roads they affect:

```rust
let source = Open511Source::builder("construction")
    .with_base_url("https://api.open511.gov.bc.ca")
    .with_status_filter("ACTIVE")
    .with_event_type_filter("CONSTRUCTION")
    .with_bootstrap_provider(bootstrap)
    .build()?;

let query = Query::cypher("construction-events")
    .query(r#"
        MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
        RETURN e.id AS event_id,
               e.headline AS headline,
               e.description AS description,
               r.name AS road,
               r.from AS from_loc,
               r.to AS to_loc,
               r.direction AS direction,
               r.state AS road_state,
               e.schedule_intervals AS schedule,
               e.created AS started,
               e.updated AS last_update
    "#)
    .from_source("construction")
    .auto_start(true)
    .enable_bootstrap(true)
    .build();
```

The continuous query keeps the dashboard in sync automatically—new construction
appears, updated schedules flow through, and completed work disappears.

### 4. Bounding-box geofence for a city

Monitor events within a geographic bounding box (e.g. greater Vancouver):

```rust
let source = Open511Source::builder("vancouver-events")
    .with_base_url("https://api.open511.gov.bc.ca")
    .with_status_filter("ACTIVE")
    .with_bbox_filter("-123.3,49.0,-122.5,49.4")  // Vancouver metro area
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

### 5. Cross-agency monitoring

Run multiple sources against different Open511 endpoints and query across them:

```rust
let bc_source = Open511Source::builder("bc-events")
    .with_base_url("https://api.open511.gov.bc.ca")
    .with_status_filter("ACTIVE")
    .with_bootstrap_provider(bc_bootstrap)
    .build()?;

let ab_source = Open511Source::builder("ab-events")
    .with_base_url("https://511.alberta.ca/api")
    .with_status_filter("ACTIVE")
    .with_bootstrap_provider(ab_bootstrap)
    .build()?;

// Query against the BC source
let bc_query = Query::cypher("bc-closures")
    .query(r#"
        MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
        WHERE r.state = 'CLOSED'
        RETURN e.id AS event_id, r.name AS road, e.severity AS severity
    "#)
    .from_source("bc-events")
    .auto_start(true)
    .enable_bootstrap(true)
    .build();

// Same query against the Alberta source
let ab_query = Query::cypher("ab-closures")
    .query(r#"
        MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
        WHERE r.state = 'CLOSED'
        RETURN e.id AS event_id, r.name AS road, e.severity AS severity
    "#)
    .from_source("ab-events")
    .auto_start(true)
    .enable_bootstrap(true)
    .build();
```

### 6. Event-to-area analysis

Query which geographic districts have the most active events:

```rust
let query = Query::cypher("area-events")
    .query(r#"
        MATCH (e:RoadEvent)-[:IN_AREA]->(a:Area)
        WHERE e.status = 'ACTIVE'
        RETURN a.name AS district,
               a.id AS area_id,
               count(e) AS active_events
    "#)
    .from_source("road-events")
    .auto_start(true)
    .enable_bootstrap(true)
    .build();
```

As events are added or cleared in a district, the count updates reactively.

## Graph Model

The source produces three node types and two relationship types:

| Label | Description | Key Properties |
|---|---|---|
| `RoadEvent` | A road event (incident, construction, etc.) | `id`, `status`, `headline`, `severity`, `event_type`, `description`, `created`, `updated` |
| `Road` | An affected road segment | `name`, `from`, `to`, `direction`, `state`, `delay` |
| `Area` | A geographic district (shared across events) | `id`, `name` |
| `AFFECTS_ROAD` | `(RoadEvent)→(Road)` | `event_id` |
| `IN_AREA` | `(RoadEvent)→(Area)` | — |

For the full property reference, see [GRAPH_SCHEMA.md](GRAPH_SCHEMA.md).

## Limitations

- **Polling only** — Open511 does not provide push-based change notifications.
  Minimum practical poll interval is ~30 seconds to avoid rate limiting.
- **Delete detection requires full sweeps** — events removed between incremental
  polls are only detected during the next full sweep cycle.
- **No historical data** — the API serves currently active events only. The
  bootstrap loads whatever the API returns at startup time.
- **Area deduplication is in-memory** — if the source restarts, previously emitted
  `Area` nodes may be re-emitted as inserts (harmless but redundant).

## Testing

```bash
# Unit tests
cargo test -p drasi-source-open511

# Integration test (requires network access to API or mock server)
cargo test -p drasi-source-open511 -- --ignored --nocapture

# Lint
cargo clippy -p drasi-source-open511 --all-targets -- -D warnings
```

## Related

- [Bootstrap provider](../../bootstrappers/open511/) — loads initial snapshot
- [Getting started example](../../examples/lib/open511-getting-started/) — runnable demo
- [GRAPH_SCHEMA.md](GRAPH_SCHEMA.md) — complete graph model reference
