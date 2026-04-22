# HERE Traffic Source

The HERE Traffic source polls HERE Traffic API v7 for flow and incident data and emits
graph changes into Drasi. It creates `TrafficSegment` and `TrafficIncident` nodes and
optionally `AFFECTS` relationships based on proximity.

## Overview

**Key capabilities**

- Polls HERE Traffic API flow and incidents endpoints
- Detects changes between polling intervals (insert/update/delete)
- Builds `AFFECTS` relationships when incidents are near segments
- Uses StateStore to persist the last successful poll timestamp
- Handles rate limits with exponential backoff and `Retry-After`

## Configuration

### Builder Pattern — API Key (simplest)

```rust
use drasi_source_here_traffic::{HereTrafficSource, Endpoint, StartFrom};
use std::time::Duration;

let source = HereTrafficSource::builder("berlin-traffic", "YOUR_KEY", "52.5,13.3,52.6,13.5")
    .with_polling_interval(Duration::from_secs(60))
    .with_endpoints(vec![Endpoint::Flow, Endpoint::Incidents])
    .with_flow_change_threshold(0.5)
    .with_speed_change_threshold(5.0)
    .with_incident_match_distance_meters(500.0)
    .with_start_from(StartFrom::Now)
    .build()?;
```

### Builder Pattern — OAuth 2.0 (more secure)

```rust
use drasi_source_here_traffic::{HereTrafficSource, Endpoint};
use std::time::Duration;

let source = HereTrafficSource::builder_oauth(
        "berlin-traffic",
        "YOUR_ACCESS_KEY_ID",
        "YOUR_ACCESS_KEY_SECRET",
        "52.5,13.3,52.6,13.5",
    )
    .with_polling_interval(Duration::from_secs(60))
    .with_endpoints(vec![Endpoint::Flow, Endpoint::Incidents])
    .build()?;
```

### Configuration Struct

```rust
use drasi_source_here_traffic::{HereTrafficConfig, HereTrafficSource};

let config = HereTrafficConfig::new("YOUR_KEY", "52.5,13.3,52.6,13.5");
let source = HereTrafficSource::new("berlin-traffic", config)?;
```

### YAML (DrasiServer) — API Key

```yaml
apiVersion: v1
kind: Source
name: berlin-traffic
spec:
  kind: HereTraffic
  properties:
    apiKey: ${HERE_API_KEY}
    boundingBox: "52.5,13.3,52.6,13.5"
    pollingInterval: 60
    endpoints:
      - flow
      - incidents
    flowChangeThreshold: 0.5
    speedChangeThreshold: 5.0
    incidentMatchDistanceMeters: 500.0
```

### YAML (DrasiServer) — OAuth 2.0

```yaml
apiVersion: v1
kind: Source
name: berlin-traffic
spec:
  kind: HereTraffic
  properties:
    accessKeyId: ${HERE_ACCESS_KEY_ID}
    accessKeySecret: ${HERE_ACCESS_KEY_SECRET}
    boundingBox: "52.5,13.3,52.6,13.5"
    pollingInterval: 60
```

## Configuration Options

### Authentication (choose one)

| Field | Type | Description |
|------|------|-------------|
| `apiKey` | string | Simple API key (query parameter auth) |
| `accessKeyId` | string | OAuth 2.0 access key ID (requires `accessKeySecret`) |
| `accessKeySecret` | string | OAuth 2.0 access key secret (requires `accessKeyId`) |
| `tokenUrl` | string | OAuth 2.0 token endpoint (default: HERE production) |

### Source Settings

| Field | Type | Default | Description |
|------|------|---------|-------------|
| `boundingBox` | string | **required** | `lat1,lon1,lat2,lon2` |
| `pollingInterval` | u64 | 60 | Poll interval (seconds) |
| `endpoints` | array | `[flow, incidents]` | Endpoints to poll |
| `flowChangeThreshold` | f64 | 0.5 | Jam factor delta to trigger updates |
| `speedChangeThreshold` | f64 | 5.0 | Speed delta (km/h) to trigger updates |
| `incidentMatchDistanceMeters` | f64 | 500.0 | Distance threshold for `AFFECTS` |
| `startFrom` | object | `now` | Start behavior (logged only) |
| `baseUrl` | string | HERE API URL | Override for testing |

## Data Model Mapping

### Nodes

- **TrafficSegment**
  - `id`: `segment_{lat}_{lon}` (rounded to 5 decimals)
  - `road_name`, `current_speed`, `free_flow_speed`, `jam_factor`, `confidence`
  - `functional_class`, `length_meters`, `latitude`, `longitude`, `last_updated`

- **TrafficIncident**
  - `id`: incident ID from HERE
  - `type`, `severity`, `description`, `status`, `start_time`, `end_time`
  - `latitude`, `longitude`

### Relationships

- **AFFECTS** (Incident → Segment)
  - Created when incident and segment are within `incident_match_distance_meters`
  - Includes `distance_meters` property

## Change Detection Rules

- **Flow**:
  - New segment → INSERT
  - Jam factor or speed change ≥ thresholds → UPDATE
  - Segment missing → DELETE
- **Incidents**:
  - New incident → INSERT
  - Property changes → UPDATE
  - Incident missing → DELETE

## State Store

The source stores the last successful poll timestamp under key `last_poll_timestamp`
in the StateStore partition for the source ID.

## Rate Limiting

If HERE API responds with `429`, the client backs off:

- Respects `Retry-After` header when present
- Exponential backoff: 1s → 2s → 4s (max 60s)

## Bootstrap Support

Use `drasi-bootstrap-here-traffic` for initial snapshots.

```rust
use drasi_bootstrap_here_traffic::HereTrafficBootstrapProvider;

// API key auth
let bootstrap = HereTrafficBootstrapProvider::builder()
    .with_source_id("berlin-traffic")
    .with_api_key("YOUR_KEY")
    .with_bounding_box("52.5,13.3,52.6,13.5")
    .build()?;

let source = HereTrafficSource::builder("berlin-traffic", "YOUR_KEY", "52.5,13.3,52.6,13.5")
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

```rust
// OAuth 2.0 auth
let bootstrap = HereTrafficBootstrapProvider::builder()
    .with_source_id("berlin-traffic")
    .with_oauth("ACCESS_KEY_ID", "ACCESS_KEY_SECRET")
    .with_bounding_box("52.5,13.3,52.6,13.5")
    .build()?;

let source = HereTrafficSource::builder_oauth(
        "berlin-traffic", "ACCESS_KEY_ID", "ACCESS_KEY_SECRET", "52.5,13.3,52.6,13.5",
    )
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

## Integration Tests

Run the mock HTTP integration test:

```bash
cargo test -p drasi-source-here-traffic --test integration_test -- --ignored --nocapture
```

## Troubleshooting

- **401 Unauthorized**: Verify your API key or OAuth credentials are valid and not expired.
- **429 Rate Limit**: Increase `pollingInterval` or upgrade your HERE plan.
- **No changes detected**: Expand bounding box or lower thresholds.
- **Empty results**: Check bounding box coordinates (lat,lon,lat,lon).
- **OAuth token error**: Ensure both `accessKeyId` and `accessKeySecret` are provided.
