# GTFS-RT Bootstrap Provider

Bootstrap provider for GTFS-Realtime feeds. It performs a one-time fetch and emits insert events for the current snapshot.

## Features

- Fetches configured GTFS-RT feeds once
- Emits graph insert events using the same mapping as the live source
- Supports bootstrap label filtering (`node_labels`, `relation_labels`)
- Compatible with `drasi-source-gtfs-rt`

## Basic usage

```rust
use drasi_bootstrap_gtfs_rt::GtfsRtBootstrapProvider;

let bootstrap = GtfsRtBootstrapProvider::builder()
    .with_trip_updates_url("https://www.rtd-denver.com/files/gtfs-rt/TripUpdate.pb")
    .with_vehicle_positions_url("https://www.rtd-denver.com/files/gtfs-rt/VehiclePosition.pb")
    .with_alerts_url("https://www.rtd-denver.com/files/gtfs-rt/Alerts.pb")
    .with_timeout_secs(15)
    .with_language("en")
    .build()?;
```

Attach this provider with `GtfsRtSource::builder(...).with_bootstrap_provider(bootstrap)`.

## Configuration

| Field | Description | Default |
|---|---|---|
| `trip_updates_url` | Trip updates feed URL | `None` |
| `vehicle_positions_url` | Vehicle positions feed URL | `None` |
| `alerts_url` | Alerts feed URL | `None` |
| `headers` | Optional HTTP headers | `{}` |
| `timeout_secs` | HTTP timeout in seconds | `15` |
| `language` | Preferred language for translated strings | `"en"` |

At least one feed URL must be configured.
