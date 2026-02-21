# Drasi Loki + Grafana Example

This example runs a full local observability stack with Docker Compose:

- **Loki** receives events from the Drasi Loki reaction
- **Grafana** is pre-provisioned with:
  - a Loki datasource
  - a sample dashboard showing persistent hot/cold sensor alerts, full sensor temperature history, and raw Drasi events

## What this example does

1. Starts a `MockSource` that generates `SensorReading` events
2. Runs `persistent-hot-sensors` using `drasi.trueFor(s.temperature > 24, duration({ seconds: 5 }))`
3. Runs `all-sensors` to stream all temperatures for history visualization
4. Sends both query diffs to Loki via `drasi-reaction-loki`
5. Visualizes the data in Grafana dashboard **Drasi Loki Overview** (`uid=drasi-loki-overview`)

## Prerequisites

- Docker + Docker Compose
- Rust toolchain

## Run

```bash
cd examples/lib/loki
./run.sh
```

This script:

1. Starts Grafana + Loki with `docker compose up -d`
2. Runs the Drasi example with `LOKI_ENDPOINT=http://localhost:3100`

> Note: The mock source generates temperatures in the 20-30 range. `persistent-hot-sensors` requires values to stay `> 24` for at least 5 seconds before emitting HOT transitions.

## How `drasi.trueFor` behaves in this example

The `persistent-hot-sensors` query uses:

```cypher
WHERE drasi.trueFor(s.temperature > 24, duration({ seconds: 5 }))
```

This means:

- A sensor is added to the result set only after it has continuously satisfied `temperature > 24` for at least 5 seconds.
- While the sensor stays above the threshold, updates keep the sensor in the result set.
- When it drops below the threshold, the sensor is removed from the result set, producing a DELETE.

The Loki template maps those transitions to indicator-friendly values:

- ADD/UPDATE -> `"active": 1` (HOT)
- DELETE -> `"active": 0` (COLD)

## Access Grafana

- URL: `http://localhost:3000`
- Username: `admin`
- Password: `admin`

Dashboard is automatically provisioned:

- **Drasi Loki Overview**
- UID: `drasi-loki-overview`
- Panels:
  - `sensor_1`..`sensor_5` HOT/COLD alert indicators from `persistent-hot-sensors`
  - Sensor temperature trend panel from `all-sensors`
  - Raw Drasi events logs panel (bottom)

## How the dashboard LogQL is constructed

### 1) Per-sensor HOT/COLD stat panels

Each sensor panel filters to `query_id="persistent-hot-sensors"` and that specific sensor:

```logql
last_over_time(
  {job="drasi-example",query_id="persistent-hot-sensors"}
  | json
  | sensor="sensor_1"
  | keep sensor,active
  | unwrap active [5m]
)
```

- `| json` parses the JSON log line fields (`sensor`, `active`, `event`, etc.).
- `| keep sensor,active` prevents extra parsed fields from creating duplicate series.
- `unwrap active` turns `active` into a numeric sample, and `last_over_time` picks the current state.
- Grafana value mapping renders `1` as HOT and `0` (or no value) as COLD.

### 2) Temperature history timeseries panel

The temperature graph uses `query_id="all-sensors"` so it shows all sensor temperatures, not only persistent-hot ones:

```logql
last_over_time(
  {job="drasi-example",query_id="all-sensors"}
  | json 
  | event=~"ADD|UPDATE"
  | unwrap temperature [1m]
) by (sensor)
```

- Filtering to `ADD|UPDATE` excludes DELETE rows that do not represent current temperature samples.
- Grouping `by (sensor)` gives one line per sensor.

## Useful commands

```bash
make compose-up
make run
make compose-down
```

## Verify data directly in Loki

```bash
curl -G 'http://localhost:3100/loki/api/v1/query_range' \
  --data-urlencode 'query={job="drasi-example"}'

curl -G 'http://localhost:3100/loki/api/v1/query_range' \
  --data-urlencode 'query={job="drasi-example",query_id="persistent-hot-sensors"}'

curl -G 'http://localhost:3100/loki/api/v1/query_range' \
  --data-urlencode 'query={job="drasi-example",query_id="all-sensors"}'
```
