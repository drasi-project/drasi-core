# DrasiLib Dashboard Example

A self-contained IoT sensor dashboard built with drasi-lib. MockSource generates
live temperature/humidity readings and a `CONNECTED_TO` sensor mesh; continuous
Cypher queries feed a visual dashboard with tables, charts, KPIs, markdown, and
a node-link graph.

## Architecture

```
┌──────────────────────────────────┐
│  MockSource (id: sensors)        │
│  10 sensors, 2 s interval        │
│  + live CONNECTED_TO mesh        │
└───────────────┬──────────────────┘
                │
                ▼
┌──────────────────────────────────────┐
│             5 Queries                │
├──────────────────────────────────────┤
│ 1. all-sensors     — Full table      │
│ 2. hot-sensors     — Temperature     │
│ 3. humid-sensors   — Humidity        │
│ 4. sensor-overview — Aggregates      │
│ 5. sensor-mesh     — CONNECTED_TO    │
└───────────────┬──────────────────────┘
                │
                ▼
┌──────────────────────────────────────┐
│       Dashboard Reaction             │
│    (web UI on port 3000)             │
│                                      │
│  • Predefined IoT Sensor Monitor     │
│  • Table, KPI, bar, gauge, markdown  │
│  • Graph widget of the live mesh     │
└──────────────────────────────────────┘
```

## Running

```bash
# Using the run script
./run.sh

# Or directly with cargo
cargo run

# With debug logging
RUST_LOG=debug cargo run
```

Then open **http://localhost:3000** in your browser. The predefined
`iot-monitor` dashboard is seeded on first start.

## Queries

### all-sensors
All current sensor readings — **Table** and **KPI**.

```cypher
MATCH (s:SensorReading)
RETURN s.sensor_id AS sensor_id,
       s.temperature AS temperature,
       s.humidity AS humidity,
       s.timestamp AS timestamp
```

### hot-sensors
Sensors above 27 °C — **KPI** alert count.

```cypher
MATCH (s:SensorReading)
WHERE s.temperature > 27
RETURN s.sensor_id AS sensor_id,
       s.temperature AS temperature
```

### humid-sensors
Sensors with humidity above 50 % — **KPI** / alerts.

```cypher
MATCH (s:SensorReading)
WHERE s.humidity > 50
RETURN s.sensor_id AS sensor_id,
       s.humidity AS humidity
```

### sensor-overview
All sensors for aggregate stats — **Bar Chart**, **Gauge**, **Markdown**.

```cypher
MATCH (s:SensorReading)
RETURN s.sensor_id AS sensor_id,
       s.temperature AS temperature,
       s.humidity AS humidity
```

### sensor-mesh
Live `CONNECTED_TO` edges — **Graph** widget. Each row is one edge; nodes are
inferred from `source` / `target`. `weight` sizes the links; mesh topology
changes as MockSource updates `strength` and rewires chords.

```cypher
MATCH (a:SensorReading)-[r:CONNECTED_TO]->(b:SensorReading)
RETURN a.sensor_id AS source,
       b.sensor_id AS target,
       a.temperature AS sourceTemp,
       r.strength AS weight
```

## HTTP Endpoints

| Port | Method | Endpoint | Description |
|------|--------|----------|-------------|
| 3000 | GET | `/` | Dashboard web UI |
| 3000 | WS | `/ws` | Real-time data (WebSocket) |
| 3000 | GET | `/api/dashboards` | List saved dashboards |
| 3000 | GET | `/api/queries` | List available query IDs |

## Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Crate manifest (standalone workspace) |
| `main.rs` | Example source code |
| `run.sh` | Shell script to run the example |
| `README.md` | This file |

## Dependencies

- `drasi-lib` — Core library
- `drasi-source-mock` — Synthetic IoT source
- `drasi-reaction-dashboard` — Dashboard reaction component
