# Drasi Loki + Grafana Example

This example runs a full local observability stack with Docker Compose:

- **Loki** receives events from the Drasi Loki reaction
- **Grafana** is pre-provisioned with:
  - a Loki datasource
  - a sample dashboard showing Drasi event logs and per-operation event rate

## What this example does

1. Starts a `MockSource` that generates `SensorReading` events
2. Runs a query that filters readings where `temperature > 24`
3. Sends query diffs to Loki via `drasi-reaction-loki`
4. Visualizes the data in Grafana dashboard **Drasi Loki Overview** (`uid=drasi-loki-overview`)

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

> Note: The mock source generates temperatures in the 20-30 range. The query uses `> 24` so data continuously appears in Loki/Grafana while the example is running.

## Access Grafana

- URL: `http://localhost:3000`
- Username: `admin`
- Password: `admin`

Dashboard is automatically provisioned:

- **Drasi Loki Overview**
- UID: `drasi-loki-overview`
- Panels:
  - `sensor_1`..`sensor_5` temperature gauges
  - Sensor temperature trend panel
  - Raw Drasi events logs panel (bottom)

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
```
