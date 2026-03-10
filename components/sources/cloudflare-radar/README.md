# Cloudflare Radar Source

## Overview

The Cloudflare Radar source turns [Cloudflare Radar](https://radar.cloudflare.com/) into a live, queryable graph inside Drasi. It polls the Radar API at a configurable interval and emits graph changes for internet outages, BGP hijacks and leaks, HTTP/L7/L3 attack summaries, domain rankings, and DNS query analytics — letting you write continuous Cypher queries that react the moment the internet landscape changes.

### Why use this?

Cloudflare Radar aggregates telemetry from roughly 20 % of all internet traffic. By feeding that data into Drasi you can build **reactive** systems that go far beyond static dashboards:

- **NOC alerting** — trigger PagerDuty / Slack when a BGP hijack targets *your* prefixes.
- **Supply-chain risk** — watch whether key SaaS domains drop out of the top-100 rankings.
- **Regulatory reporting** — continuously log internet outages affecting specific countries.
- **Threat-intel enrichment** — correlate L7 attack spikes with your own WAF logs in real time.

## Prerequisites

- Cloudflare API token with **Account → Radar → Read** permission.
- Network access to `https://api.cloudflare.com/client/v4` (or a custom base URL for testing).
- Optional: a `StateStoreProvider` to persist polling state across restarts.

## Configuration

Configure via `CloudflareRadarSource::builder`:

| Method | Required | Default | Description |
|--------|----------|---------|-------------|
| `with_api_token` | ✅ | — | Cloudflare API bearer token |
| `with_api_base_url` | | `https://api.cloudflare.com/client/v4` | API base URL |
| `with_poll_interval_secs` | | `300` | Seconds between polls (must be > 0) |
| `with_category(name, bool)` | ✅ (≥1) | all `false` | Enable individual data categories |
| `with_location_filter` | | `None` | ISO country codes to scope outages/BGP events |
| `with_bgp_asn_filter` | | `None` | ASNs to scope BGP events |
| `with_hijack_min_confidence` | | `None` | Minimum confidence score for BGP hijacks |
| `with_ranking_limit` | | `100` | Number of top domains to track |
| `with_dns_domains` | if `dns` on | `None` | Domains to track DNS query analytics for |
| `with_analytics_date_range` | | `1d` | Time window for summary endpoints |
| `with_event_date_range` | | `7d` | Time window for event endpoints |
| `with_start_behavior` | | `StartFromNow` | How to treat the first poll |
| `with_state_store` | | `None` | Persist poll cursor across restarts |
| `with_bootstrap_provider` | | `None` | Bootstrap provider for initial load |
| `with_auto_start` | | `true` | Start polling automatically |

**Categories** (pass to `with_category`):
`outages`, `bgp_hijacks`, `bgp_leaks`, `http_traffic`, `attacks_l7`, `attacks_l3`, `domain_rankings`, `dns`

### Start behavior

| Variant | Behavior |
|---------|----------|
| `StartFromNow` *(default)* | First poll seeds internal state silently; changes are emitted from the **second** poll onward. |
| `StartFromBeginning` | Emit all data as inserts on the very first poll. |
| `StartFromTimestamp(i64)` | Emit only events whose timestamp ≥ the given Unix timestamp. Useful for resuming from a known point. |

## Data Model

### Nodes

| Label | Key Properties |
|-------|----------------|
| `Outage` | `scope`, `outageCause`, `outageType`, `startDate`, `endDate`, `eventType`, `linkedUrl` |
| `Location` | `code` (ISO 3166-1 alpha-2) |
| `AutonomousSystem` | `asn` |
| `BgpHijack` | `eventId`, `duration`, `confidenceScore`, `hijackMsgCount`, `peerIpCount`, `isStale` |
| `BgpLeak` | `eventId`, `leakCount`, `leakType`, `detectedTs`, `originCount`, `peerCount`, `prefixCount`, `finished` |
| `Prefix` | `prefix` (e.g. `1.0.0.0/24`) |
| `HttpTraffic` | `series`, `desktop`, `mobile`, `other` |
| `AttackL7` | `series`, `ddos`, `waf`, `ipReputation`, `accessRules`, `botManagement`, `apiShield`, `dataLossPrevention` |
| `AttackL3` | `series`, `udp`, `tcp`, `icmp`, `gre` |
| `Domain` | `name` |
| `DomainRanking` | `rank`, `domain` |
| `DnsQuerySummary` | `domain`, `topLocations` |

### Relationships

| Pattern | Description |
|---------|-------------|
| `(Outage)-[:AFFECTS_LOCATION]->(Location)` | Countries affected by an outage |
| `(Outage)-[:AFFECTS_ASN]->(AutonomousSystem)` | ASNs affected by an outage |
| `(BgpHijack)-[:HIJACKED_BY]->(AutonomousSystem)` | The hijacking AS |
| `(BgpHijack)-[:VICTIM_ASN]->(AutonomousSystem)` | The victim AS |
| `(BgpHijack)-[:TARGETS_PREFIX]->(Prefix)` | Hijacked IP prefix |
| `(BgpLeak)-[:LEAKED_BY]->(AutonomousSystem)` | The leaking AS |
| `(Domain)-[:RANKED_AS]->(DomainRanking)` | Current ranking for a domain |

## Use Cases & Example Queries

### 1. Internet Outage Monitor

Track outages in real time and alert when one hits a country you care about.

```rust
let source = CloudflareRadarSource::builder("radar")
    .with_api_token(token)
    .with_category("outages", true)
    .with_location_filter(vec!["US".into(), "DE".into(), "JP".into()])
    .with_poll_interval_secs(120)
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

```cypher
MATCH (o:Outage)-[:AFFECTS_LOCATION]->(loc:Location)
WHERE loc.code IN ['US', 'DE', 'JP']
RETURN o.scope AS scope,
       o.outageCause AS cause,
       o.startDate AS started,
       loc.code AS country
```

### 2. BGP Hijack Early Warning

Detect BGP hijacks targeting your organization's prefixes or ASN.

```rust
let source = CloudflareRadarSource::builder("radar")
    .with_api_token(token)
    .with_category("bgp_hijacks", true)
    .with_bgp_asn_filter(vec![13335, 16509])   // your ASNs
    .with_hijack_min_confidence(80)
    .with_poll_interval_secs(60)
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

```cypher
MATCH (h:BgpHijack)-[:VICTIM_ASN]->(victim:AutonomousSystem)
WHERE h.confidenceScore >= 80
RETURN h.eventId AS event,
       h.confidenceScore AS confidence,
       victim.asn AS victimAsn,
       h.hijackMsgCount AS messages
```

### 3. SaaS Dependency Watchdog

Continuously verify that critical SaaS providers remain in the top-100 global rankings. A query that fires when a domain drops out can trigger an investigation workflow.

```rust
let source = CloudflareRadarSource::builder("radar")
    .with_api_token(token)
    .with_category("domain_rankings", true)
    .with_ranking_limit(200)
    .with_poll_interval_secs(600)
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

```cypher
MATCH (d:Domain)-[:RANKED_AS]->(r:DomainRanking)
WHERE d.name IN ['github.com', 'slack.com', 'aws.amazon.com']
RETURN d.name AS domain, r.rank AS currentRank
```

### 4. Attack Trend Dashboard

Build a live dashboard showing DDoS and WAF attack distribution.

```rust
let source = CloudflareRadarSource::builder("radar")
    .with_api_token(token)
    .with_category("attacks_l7", true)
    .with_category("attacks_l3", true)
    .with_analytics_date_range("1d".into())
    .with_poll_interval_secs(300)
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

```cypher
MATCH (a:AttackL7)
RETURN a.ddos AS ddosPct,
       a.waf AS wafPct,
       a.botManagement AS botPct
```

```cypher
MATCH (a:AttackL3)
RETURN a.udp AS udpPct,
       a.tcp AS tcpPct,
       a.icmp AS icmpPct
```

### 5. DNS Popularity Tracker

Monitor which countries are generating the most DNS queries for your domains.

```rust
let source = CloudflareRadarSource::builder("radar")
    .with_api_token(token)
    .with_category("dns", true)
    .with_dns_domains(vec!["example.com".into(), "api.example.com".into()])
    .with_poll_interval_secs(300)
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

```cypher
MATCH (d:DnsQuerySummary)
WHERE d.domain = 'example.com'
RETURN d.domain AS domain, d.topLocations AS locations
```

### 6. BGP Leak Detector

Track route leaks that could indicate accidental misconfigurations affecting your upstream providers.

```rust
let source = CloudflareRadarSource::builder("radar")
    .with_api_token(token)
    .with_category("bgp_leaks", true)
    .with_bgp_asn_filter(vec![13335])
    .with_poll_interval_secs(120)
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

```cypher
MATCH (l:BgpLeak)-[:LEAKED_BY]->(as:AutonomousSystem)
WHERE l.finished = false
RETURN l.eventId AS event,
       l.leakType AS type,
       l.prefixCount AS prefixes,
       as.asn AS leakerAsn
```

### 7. Device Mix Analytics

Track how your traffic splits between desktop, mobile, and other devices.

```rust
let source = CloudflareRadarSource::builder("radar")
    .with_api_token(token)
    .with_category("http_traffic", true)
    .with_poll_interval_secs(300)
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

```cypher
MATCH (h:HttpTraffic)
RETURN h.desktop AS desktopPct,
       h.mobile AS mobilePct,
       h.other AS otherPct
```

## Change Detection

| Category | Mechanism | Details |
|----------|-----------|---------|
| Outages, Hijacks, Leaks | **ID-based** | Each event is tracked by a unique ID; new IDs → insert, gone IDs → delete |
| HTTP / L7 / L3 summaries | **Hash-based** | A deterministic hash of the full response is compared across polls |
| Domain rankings | **Diff-based** | Current rank set compared to previous; supports insert, update, delete |
| DNS summaries | **Per-domain hash** | Each domain's top-locations response is hashed independently |

## Limitations

- **Polling only** — Cloudflare Radar has no streaming or webhook API.
- **Rate limits** — Aggressive polling (< 60 s) may trigger Cloudflare API rate limiting. The source retries with exponential backoff on 429 / 5xx, but sustained overuse can still fail.
- **Eventual consistency** — Radar data is aggregated; small delays between a real-world event and its appearance in the API are normal.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Cloudflare Radar API token is required` | Empty or missing token | Set `with_api_token` to a valid token |
| `DNS category enabled but no dns_domains provided` | `dns` enabled without domains | Provide `with_dns_domains(vec![...])` |
| `At least one category must be enabled` | All categories disabled | Enable at least one via `with_category` |
| `poll_interval_secs must be greater than 0` | Interval set to 0 | Use a value ≥ 1 (recommended ≥ 60) |
| 429 / rate limiting in logs | Polling too aggressively | Increase `poll_interval_secs` or reduce enabled categories |
| State deserialization warnings | Corrupted or incompatible persisted state | Safe to ignore — state resets to defaults; check `StateStore` health |
| No changes detected after first poll | `StartFromNow` behavior | Expected — first poll is silent; changes appear on the next cycle |
