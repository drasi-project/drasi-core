# HERE Traffic Getting Started

Monitor real-time traffic conditions using the [HERE Traffic API v7](https://developer.here.com/documentation/traffic-api/dev_guide/index.html) as a Drasi continuous query source. The source polls flow and incident data for a geographic bounding box and emits graph changes — letting you write Cypher queries that react to congestion spikes, new accidents, and clearing conditions as they happen.

## What You Get

The source creates three types of graph elements that you can query against:

| Element | Label | Key properties |
|---------|-------|---------------|
| **Flow segment** | `TrafficSegment` | `road_name`, `jam_factor` (0–10), `current_speed`, `free_flow_speed`, `confidence`, `latitude`, `longitude` |
| **Incident** | `TrafficIncident` | `id`, `type` (ACCIDENT, CONSTRUCTION, …), `severity` (LOW/MEDIUM/HIGH/CRITICAL), `description`, `status`, `start_time` |
| **Proximity link** | `AFFECTS` (relationship) | `distance_meters` — auto-created when an incident is within 500 m of a segment |

Changes are detected by comparing successive polls: new elements produce **ADD**, changed values produce **UPDATE**, and elements that disappear produce **DELETE** events.

## Prerequisites

- **HERE credentials** — pick one:
  - **API key** — the simplest option; get one at <https://developer.here.com/>
  - **OAuth 2.0** — more secure; create an app in the [HERE portal](https://platform.here.com/) and generate an Access Key ID + Secret
- Rust toolchain (for building the example)
- `curl` and `jq` (for the helper scripts)

## Quick Start

### Option A — API Key

```bash
export HERE_API_KEY=your_key_here
./quickstart.sh
```

### Option B — OAuth 2.0

```bash
export HERE_ACCESS_KEY_ID=your_access_key_id
export HERE_ACCESS_KEY_SECRET=your_access_key_secret
./quickstart.sh
```

You can also set `HERE_BBOX` to target a different area (default: central Berlin `52.5,13.3,52.6,13.5`).

## Example Queries

The example ships with two queries out of the box. Below are additional queries you can try by editing `main.rs`.

### 1. Congestion Alerts — "Show me gridlock"

Return every segment where `jam_factor` exceeds 5 (heavy congestion on a 0–10 scale):

```cypher
MATCH (s:TrafficSegment)
WHERE s.jam_factor > 5.0
RETURN s.road_name AS road,
       s.jam_factor AS jam_factor,
       s.current_speed AS speed_kmh
```

Because this is a continuous query, you'll see results **appear** when a road becomes congested and **disappear** when it clears.

### 2. High-Severity Incidents

```cypher
MATCH (s:TrafficIncident)
WHERE s.severity = 'HIGH' OR s.severity = 'CRITICAL'
RETURN s.id AS incident_id,
       s.type AS kind,
       s.description AS what
```

### 3. Incidents Affecting Congested Segments

Use the auto-generated `AFFECTS` relationship to correlate incidents with nearby congested roads:

```cypher
MATCH (i:TrafficIncident)-[:AFFECTS]->(s:TrafficSegment)
WHERE s.jam_factor > 3.0
RETURN i.id AS incident,
       i.type AS type,
       s.road_name AS road,
       s.jam_factor AS congestion
```

### 4. Slow-Speed Detection

Flag segments where traffic has dropped below 20 km/h on roads that normally allow 50+:

```cypher
MATCH (s:TrafficSegment)
WHERE s.current_speed < 20.0 AND s.free_flow_speed > 50.0
RETURN s.road_name AS road,
       s.current_speed AS actual,
       s.free_flow_speed AS normal
```

### 5. Active Construction Zones

```cypher
MATCH (i:TrafficIncident)
WHERE i.type = 'CONSTRUCTION' AND i.status = 'ACTIVE'
RETURN i.id AS id,
       i.description AS description,
       i.start_time AS since
```

### 6. Count Incidents by Type

```cypher
MATCH (i:TrafficIncident)
RETURN i.type AS type, count(i) AS total
```

### 7. Nearby Incidents (< 200 m from a segment)

```cypher
MATCH (i:TrafficIncident)-[a:AFFECTS]->(s:TrafficSegment)
WHERE a.distance_meters < 200
RETURN i.id AS incident,
       s.road_name AS road,
       a.distance_meters AS distance_m
```

## Popular Bounding Boxes

| City | `HERE_BBOX` |
|------|------------|
| Berlin (central) | `52.50,13.30,52.55,13.45` |
| Berlin (wide) | `52.40,13.10,52.60,13.60` |
| London (central) | `51.48,-0.18,51.54,-0.05` |
| New York (Manhattan) | `40.70,-74.02,40.80,-73.93` |
| Los Angeles (downtown) | `33.95,-118.30,34.10,-118.15` |
| Tokyo (central) | `35.65,139.70,35.72,139.80` |
| São Paulo (central) | `-23.60,-46.70,-23.50,-46.60` |
| Sydney (CBD) | `-33.88,151.18,-33.85,151.22` |

Larger boxes return more segments but consume more API quota per poll.

## Configuration Tuning

| Parameter | Effect | Guidance |
|-----------|--------|----------|
| `polling_interval` | How often (seconds) the API is polled | HERE free tier allows ~250 k tx/month. At 60 s that's ~43 k/month per endpoint. Lower values give faster updates but use more quota. |
| `flow_change_threshold` | Minimum `jam_factor` delta to report an UPDATE | Default `0.5`. Set to `0.0` for every fluctuation, or `1.0`+ to reduce noise. |
| `speed_change_threshold` | Minimum speed delta (km/h) to report an UPDATE | Default `5.0`. Lower values capture smaller speed changes. |
| `incident_match_distance_meters` | Radius for auto-creating `AFFECTS` relationships | Default `500`. Use `1000`+ for highways, `200` for dense urban grids. |

## Understanding the Output

When the example runs you'll see log lines like:

```
[+] Unter den Linden jam=6.2 speed=12.3          ← new congested segment appeared
[~] Friedrichstraße jam=5.1 -> 7.8                ← congestion worsened
[-] Kurfürstendamm removed                        ← congestion cleared
[+] Incident INC_4821: ACCIDENT                   ← new incident detected
[-] Incident INC_4821 resolved                    ← incident no longer reported
```

- **`[+]`** = ADD — the element entered the query's result set (new, or newly matching the WHERE clause)
- **`[~]`** = UPDATE — a property changed enough to cross the configured threshold
- **`[-]`** = DELETE — the element left the result set (removed from API, or no longer matching)

## How to Verify It's Working

1. Start the example with `./quickstart.sh`
2. Watch for `[+]` lines within the first polling interval (60 s by default)
3. Wait 2–3 more intervals — you should see `[~]` updates as traffic conditions shift
4. Run `./test-updates.sh` for a guided verification

If you see only bootstrap output and no subsequent changes, try a larger bounding box or lower thresholds.

## Helper Scripts

| Script | Purpose |
|--------|---------|
| `setup.sh` | Validate credentials, test API connectivity, build the example |
| `quickstart.sh` | Run `setup.sh` then launch the example |
| `diagnose.sh` | Check HERE API connectivity and print response metadata |
| `test-updates.sh` | Tail the example output and highlight change events |

## Project Layout

```
here-traffic-getting-started/
├── Cargo.toml          # Dependencies
├── main.rs             # DrasiLib setup with two queries and two LogReactions
├── README.md           # This file
├── setup.sh            # Credential validation + build
├── quickstart.sh       # One-command launch
├── diagnose.sh         # API connectivity check
└── test-updates.sh     # Change detection verification
```

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|-------------|-----|
| **401 Unauthorized** | Invalid or expired credentials | Regenerate your API key or OAuth credentials in the HERE portal |
| **429 Too Many Requests** | Rate limit exceeded | Increase `polling_interval` or upgrade your HERE plan |
| **No changes after bootstrap** | Bounding box too small or thresholds too high | Use a wider bbox or set `flow_change_threshold: 0.1` |
| **Empty results** | Bad bbox format or no coverage | Verify format is `lat1,lon1,lat2,lon2` and the area has HERE traffic coverage |
| **OAuth token error** | Missing or mismatched credentials | Ensure *both* `HERE_ACCESS_KEY_ID` and `HERE_ACCESS_KEY_SECRET` are set |
| **Timeout on startup** | Network or DNS issue | Run `./diagnose.sh` to test raw API connectivity |

## Further Reading

- [HERE Traffic API v7 docs](https://developer.here.com/documentation/traffic-api/dev_guide/index.html)
- [HERE Traffic source README](../../../components/sources/here-traffic/README.md) — full configuration reference
- [HERE Traffic bootstrap README](../../../components/bootstrappers/here-traffic/README.md) — bootstrap provider details
- [Drasi continuous queries](https://drasi.io/) — how Drasi processes streaming graph changes
