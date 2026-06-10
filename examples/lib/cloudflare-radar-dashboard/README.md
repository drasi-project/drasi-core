# Cloudflare Radar Dashboard Example

## What This Demonstrates

A self-contained application that connects to Cloudflare Radar, runs three continuous Cypher queries against the live data, and exposes a tiny HTTP API so you can watch the results evolve in real time.

| Query ID | What it tracks | Cypher |
|----------|---------------|--------|
| `active-outages` | Ongoing internet outages and their affected countries | `MATCH (o:Outage)-[:AFFECTS_LOCATION]->(loc:Location) RETURN ...` |
| `hijacks` | High-confidence BGP hijack events with victim and hijacker ASNs | `MATCH (h:BgpHijack)-[:VICTIM_ASN]->(v:AutonomousSystem) RETURN ...` |
| `top-domains` | Top-ranked global domains and their current position | `MATCH (d:Domain)-[:RANKED_AS]->(r:DomainRanking) RETURN ...` |

The app bootstraps from Cloudflare's API on startup so queries return data immediately, then polls for live changes.

## Prerequisites

- **Rust toolchain** with `cargo` (1.83+)
- **Cloudflare API token** — create one in the [Cloudflare dashboard](https://dash.cloudflare.com/profile/api-tokens) with **Account → Radar → Read** permission

## Quick Start

```bash
# 1. Set your token
export CF_RADAR_TOKEN=your-token-here

# 2. Build & run
./setup.sh       # builds the binary and checks prerequisites
./quickstart.sh  # starts the dashboard on port 8080
```

## Querying Results

While the app is running, fetch current results from any query:

```bash
# Active outages
curl -s http://localhost:8080/queries/active-outages/results | jq .

# BGP hijacks
curl -s http://localhost:8080/queries/hijacks/results | jq .

# Top domains
curl -s http://localhost:8080/queries/top-domains/results | jq .
```

### Example response — `active-outages`

```json
[
  {
    "scope": "Cable cut between Djibouti and Yemen",
    "cause": "Cable Cut",
    "started": "2025-01-15T08:30:00Z",
    "country": "YE"
  }
]
```

### Example response — `top-domains`

```json
[
  { "domain": "google.com",   "rank": 1 },
  { "domain": "facebook.com", "rank": 2 },
  { "domain": "apple.com",    "rank": 3 }
]
```

## Watch for Live Changes

The polling helper script fetches results repeatedly so you can observe changes as they arrive:

```bash
# Polls every 60 s by default
./test-updates.sh

# Custom interval
CF_RADAR_TEST_INTERVAL_SECS=30 ./test-updates.sh
```

You'll see timestamped output each cycle — useful for demos or verifying that change detection is working.

## Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `CF_RADAR_TOKEN` | ✅ | — | Cloudflare API token |
| `CF_RADAR_API_BASE_URL` | | `https://api.cloudflare.com/client/v4` | Override for testing / mocking |
| `CF_RADAR_POLL_INTERVAL_SECS` | | `300` | Seconds between polls |
| `CF_RADAR_TEST_INTERVAL_SECS` | | `60` | Seconds between `test-updates.sh` cycles |

## Helper Scripts

| Script | Purpose |
|--------|---------|
| `setup.sh` | Builds the example binary, validates Rust toolchain and env vars |
| `quickstart.sh` | Runs the dashboard; prompts to free port 8080 if occupied |
| `diagnose.sh` | Prints environment info and current query results — useful for debugging |
| `test-updates.sh` | Continuously polls the results API and prints timestamped output |

## Scenarios to Try

### 1. Outage Alerting Workflow

Run the dashboard, then simulate an alerting pipeline:

```bash
# In terminal 1 — run the dashboard
./quickstart.sh

# In terminal 2 — poll for outages and pipe to a webhook
while true; do
  outages=$(curl -sf http://localhost:8080/queries/active-outages/results)
  count=$(echo "$outages" | jq 'length')
  if [ "$count" -gt 0 ]; then
    echo "[$(date)] $count active outage(s) detected"
    # curl -X POST https://hooks.slack.com/... -d "$outages"
  fi
  sleep 120
done
```

### 2. BGP Security Monitoring

Watch for hijack events and filter by confidence score:

```bash
curl -s http://localhost:8080/queries/hijacks/results | \
  jq '[.[] | select(.confidence >= 80)]'
```

### 3. Domain Ranking Changes

Compare snapshots over time to see which domains moved:

```bash
# Save a baseline
curl -s http://localhost:8080/queries/top-domains/results | jq . > baseline.json

# Wait for the next polling cycle, then diff
sleep 310
curl -s http://localhost:8080/queries/top-domains/results | jq . > current.json
diff baseline.json current.json
```

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `CF_RADAR_TOKEN not set` | Missing env var | `export CF_RADAR_TOKEN=...` |
| Port 8080 in use | Another process on that port | Let `quickstart.sh` kill it, or stop manually |
| Empty results on first query | First poll hasn't completed | Wait ~30 s and retry |
| 401 errors in logs | Invalid or expired token | Regenerate token in Cloudflare dashboard |
| No changes between polls | Data hasn't changed upstream | Expected for quiet periods; try reducing poll interval for testing |
