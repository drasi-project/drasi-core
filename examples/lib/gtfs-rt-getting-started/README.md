# GTFS-RT Getting Started Dashboard

This standalone example runs a Drasi GTFS-Realtime source and serves a live dashboard UI.

## What it shows

- **Live Vehicle Map** — Leaflet.js map with color-coded markers (green = on time, yellow = delayed, red = very delayed)
- **Delay Leaderboard** — auto-updating table of most delayed trips sorted by delay
- **Active Alerts** — service alert cards with severity badges, cause/effect, and affected routes/stops
- **Route Statistics** — Chart.js bar chart of active vehicles per route

## Run

```bash
cd examples/lib/gtfs-rt-getting-started
cargo run
```

Open: `http://localhost:8090`

## Environment variables

| Variable | Default |
|---|---|
| `GTFS_RT_TRIP_UPDATES_URL` | `https://www.rtd-denver.com/files/gtfs-rt/TripUpdate.pb` |
| `GTFS_RT_VEHICLE_POSITIONS_URL` | `https://www.rtd-denver.com/files/gtfs-rt/VehiclePosition.pb` |
| `GTFS_RT_ALERTS_URL` | `https://www.rtd-denver.com/files/gtfs-rt/Alerts.pb` |
| `GTFS_RT_POLL_INTERVAL_SECS` | `30` |
| `GTFS_RT_TIMEOUT_SECS` | `15` |
| `GTFS_RT_LANGUAGE` | `en` |
| `GTFS_RT_DASHBOARD_HOST` | `0.0.0.0` |
| `GTFS_RT_DASHBOARD_PORT` | `8090` |

Set any feed URL to an empty string to disable that feed.

## Scripts

- `./setup.sh` — build example
- `./quickstart.sh` — run with default RTD feeds
- `./diagnose.sh` — quick HTTP diagnostics for feed URLs
- `./test-updates.sh` — poll the dashboard API state
