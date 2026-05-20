# RIS Live Web Dashboard (DrasiLib)

A live web dashboard that streams real-time BGP updates from [RIPE NCC RIS Live](https://ris-live.ripe.net/) into Drasi and visualises them in a browser.

## What It Does

1. Connects to RIPE RIS Live via WebSocket
2. Maps BGP UPDATE messages into a graph of `Peer` nodes, `Prefix` nodes, and `ROUTES` relationships
3. Runs a continuous Cypher query over the graph
4. Streams route diffs to a browser dashboard over Server-Sent Events (SSE)

No databases, no Docker, no credentials — just `cargo run` and open a browser.

## Quick Start

```bash
./quickstart.sh
```

Then open **http://localhost:3000** in your browser.

Or run directly:

```bash
cargo run
```

## Dashboard

![RIS Live Dashboard](dashboard-screenshot.png)

| Section | Description |
|---|---|
| **Stats bar** | Active routes, unique peers, unique prefixes, total events, events/sec |
| **Active Routes table** | Live-updating table of all current `(Peer)-[:ROUTES]->(Prefix)` relationships with search/filter |
| **Event Feed** | Scrolling feed of ADD / UPDATE / DELETE diffs with timestamps and change details |
| **Connection indicator** | Green dot when connected, yellow while reconnecting, red when disconnected |

## What You Can Do With It

### Watch Global BGP in Real Time

Run with the defaults to see all UPDATEs from the `rrc00` collector (Amsterdam). Within seconds you'll see hundreds of routes being announced, updated, and withdrawn across the global routing table.

### Monitor a Specific Prefix

Set `RIS_PREFIX` to track your own network's prefix and see which peers are announcing it:

```bash
RIS_PREFIX=203.0.113.0/24 cargo run
```

### Detect Route Changes

The Event Feed highlights:
- 🟢 **ADD** — A new route appeared (peer started announcing a prefix)
- 🔵 **UPDATE** — A route changed (different AS path, next-hop, or origin)
- 🔴 **DELETE** — A route was withdrawn (peer stopped announcing a prefix)

Watch for UPDATEs that show `pathlen: 4→7` — these could indicate route leaks or path inflation.

### Query the REST API

While the dashboard is running, query the current routing table programmatically:

```bash
# All current routes as JSON
curl http://localhost:3000/api/routes

# Count routes
curl -s http://localhost:3000/api/routes | python3 -c "import sys,json; print(len(json.load(sys.stdin)['routes']))"

# Find routes for a specific prefix
curl -s http://localhost:3000/api/routes | python3 -c "
import sys, json
routes = json.load(sys.stdin)['routes']
for r in routes:
    if '203.0.113' in str(r.get('prefix','')):
        print(f\"{r['prefix']} via {r['peer']} (AS{r['peer_asn']}) origin=AS{r.get('origin_asn','?')}\")
"
```

## The Cypher Query

The dashboard runs this continuous query:

```cypher
MATCH (peer:Peer)-[r:ROUTES]->(p:Prefix)
RETURN peer.peer_ip AS peer,
       peer.peer_asn AS peer_asn,
       p.prefix AS prefix,
       r.next_hop AS next_hop,
       r.origin_asn AS origin_asn,
       r.path_length AS path_length
```

This query matches the live routing graph and returns every active peer–prefix route with its metadata. Because Drasi evaluates it continuously, any change to the underlying graph (insert, update, or delete) immediately produces a diff that is streamed to the dashboard.

You can modify this query in `main.rs` to focus on specific use cases — for example, filtering by `origin_asn` to detect hijacks, or by `path_length` to find instability.

## API Endpoints

| Endpoint | Description |
|---|---|
| `GET /` | Dashboard HTML page |
| `GET /events` | SSE stream of route change diffs (named event: `route-change`) |
| `GET /api/routes` | JSON snapshot of current query results |

### SSE Event Format

Each `route-change` event contains a JSON payload:

```json
{
  "query_id": "bgp-routes",
  "diffs": [
    { "ADD": { "data": { "prefix": "203.0.113.0/24", "peer": "10.0.0.1", "peer_asn": "64500", "next_hop": "10.0.0.1", "origin_asn": 64500, "path_length": 3 } } },
    { "UPDATE": { "before": { "path_length": 3, ... }, "after": { "path_length": 5, ... }, "data": { ... } } },
    { "DELETE": { "data": { "prefix": "198.51.100.0/24", "peer": "10.0.0.1", ... } } }
  ]
}
```

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `DASHBOARD_PORT` | `3000` | Port for the web dashboard |
| `RIS_WS_URL` | `wss://ris-live.ripe.net/v1/ws/` | Override RIS Live WebSocket endpoint |
| `RIS_CLIENT_NAME` | `drasi-ris-live-dashboard` | Client name sent as query parameter |
| `RIS_HOST` | `rrc00` | Route collector filter (see [RIS collectors](https://www.ripe.net/analyse/internet-measurements/routing-information-service-ris/ris-raw-data)) |
| `RIS_MESSAGE_TYPE` | `UPDATE` | RIS message type filter |
| `RIS_PREFIX` | *(unset)* | Optional prefix filter (e.g. `203.0.113.0/24`) |
| `RIS_INCLUDE_PEER_STATE` | `true` | Process `RIS_PEER_STATE` messages |
| `RIS_RECONNECT_DELAY_SECS` | `5` | Reconnect backoff |
| `RUST_LOG` | `info` | Rust logging level (`debug` for verbose output) |

## Helper Scripts

| Script | Description |
|---|---|
| `setup.sh` | Checks prerequisites (Rust toolchain) and runs `cargo check` |
| `quickstart.sh` | Runs setup, prints active config, starts `cargo run` |
| `diagnose.sh` | Prints environment, toolchain versions, and build diagnostics |
| `test-updates.sh` | Starts the dashboard for a bounded time and verifies SSE events are delivered |

## How It Works

```
┌─────────────────┐     WebSocket      ┌──────────────┐
│   RIPE NCC      │ ◀── subscribe ──── │  RIS Live    │
│   RIS Live      │ ──▶ ris_message ─▶ │  Source      │
│   (public)      │                    │  Component   │
└─────────────────┘                    └──────┬───────┘
                                              │ SourceChange
                                              │ (Insert/Update/Delete)
                                              ▼
                                       ┌──────────────┐
                                       │  Drasi Core  │
                                       │  (Cypher     │
                                       │   Engine)    │
                                       └──────┬───────┘
                                              │ ResultDiff
                                              │ (Add/Update/Delete)
                                              ▼
                                       ┌──────────────┐    SSE     ┌──────────┐
                                       │ Application  │ ────────▶  │ Browser  │
                                       │ Reaction     │            │ Dashboard│
                                       └──────────────┘            └──────────┘
```

## Troubleshooting

**Dashboard loads but shows no data**
- Check the connection indicator in the top-left. If it's yellow/red, the SSE connection failed.
- Check the terminal for WebSocket errors. RIS Live may be temporarily unavailable.
- Try `RUST_LOG=debug cargo run` for verbose output.

**Very high event volume**
- Set `RIS_PREFIX` to filter to specific prefixes.
- Use a different `RIS_HOST` (some collectors are quieter than others).

**"Connection refused" on port 3000**
- Another process may be using port 3000. Set `DASHBOARD_PORT=3001 cargo run`.

**Compilation errors**
- Run `./diagnose.sh` to check your Rust toolchain.
- Ensure you're in the `examples/lib/ris-live-getting-started/` directory.

## Learn More

- [RIS Live documentation](https://ris-live.ripe.net/)
- [RIS Live WebSocket protocol](https://ris-live.ripe.net/manual)
- [RIPE NCC RIS collectors](https://www.ripe.net/analyse/internet-measurements/routing-information-service-ris/ris-raw-data)
- [RIS Live Source README](../../../components/sources/ris-live/README.md) — full configuration reference
- [Graph Schema Reference](../../../components/sources/ris-live/docs/graph-schema.md) — detailed node/relationship/property specification
