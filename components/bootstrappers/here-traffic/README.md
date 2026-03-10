# HERE Traffic Bootstrap Provider

Bootstrap provider for HERE Traffic API sources. It fetches the current flow and
incident data from HERE and emits INSERT bootstrap events for `TrafficSegment`
and `TrafficIncident` nodes plus `AFFECTS` relationships.

## Usage

```rust
use drasi_bootstrap_here_traffic::HereTrafficBootstrapProvider;

let provider = HereTrafficBootstrapProvider::builder()
    .with_source_id("berlin-traffic")
    .with_api_key("YOUR_KEY")
    .with_bounding_box("52.5,13.3,52.6,13.5")
    .build()?;
```

## Configuration

| Field | Type | Default | Description |
|------|------|---------|-------------|
| `api_key` | string | **required** | HERE API key |
| `bounding_box` | string | **required** | `lat1,lon1,lat2,lon2` |
| `endpoints` | array | `[flow, incidents]` | Endpoints to poll |
| `incident_match_distance_meters` | f64 | 500.0 | Distance threshold for `AFFECTS` |
| `base_url` | string | HERE API URL | Override for testing |

## Notes

- Bootstrap uses a one-time snapshot; polling interval settings are ignored.
- If both flow and incidents are enabled, relationships are created for nearby pairs.

## Troubleshooting

- **401 Unauthorized**: Verify your HERE API key.
- **429 Rate Limit**: Use a larger polling interval for the source or upgrade plan.
