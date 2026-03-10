# Cloudflare Radar Bootstrap Provider

## Overview

The Cloudflare Radar bootstrap provider loads an initial snapshot from the Cloudflare Radar API so that continuous queries already have data to work with *before* the first live polling cycle completes. Without a bootstrap provider, queries would return empty results until the source's second poll (under the default `StartFromNow` behavior).

## How It Works

1. When a continuous query starts, Drasi asks the bootstrap provider for elements that match the query's required labels and relationship types.
2. The bootstrapper makes a single pass through the configured Cloudflare Radar endpoints, converts the responses into graph elements (nodes + relationships), and returns them.
3. From that point forward, the **source** takes over with polling-based change detection.

The bootstrapper uses the same retry/backoff logic as the source (up to 4 attempts with exponential backoff on 429 / 5xx / network errors), so transient API failures during startup do not crash the system.

## Configuration

Configure via `CloudflareRadarBootstrapProvider::builder`:

| Method | Required | Default | Description |
|--------|----------|---------|-------------|
| `with_api_token` | ✅ | — | Cloudflare API bearer token |
| `with_api_base_url` | | `https://api.cloudflare.com/client/v4` | API base URL |
| `with_category(name, bool)` | ✅ (≥1) | all `false` | Enable data categories to bootstrap |
| `with_location_filter` | | `None` | ISO country codes to scope outages/BGP events |
| `with_bgp_asn_filter` | | `None` | ASNs to scope BGP events |
| `with_hijack_min_confidence` | | `None` | Minimum confidence score for BGP hijacks |
| `with_ranking_limit` | | `100` | Number of top domains to load |
| `with_dns_domains` | if `dns` on | `None` | Domains to load DNS analytics for |
| `with_analytics_date_range` | | `1d` | Time window for summary endpoints |
| `with_event_date_range` | | `7d` | Time window for event endpoints |

> **Tip:** Keep the bootstrapper's categories and filters aligned with the source's. If they diverge, the bootstrapper may load data the source never updates — or vice versa.

## Usage Examples

### Minimal — outages only

```rust
use drasi_bootstrap_cloudflare_radar::CloudflareRadarBootstrapProvider;

let bootstrap = CloudflareRadarBootstrapProvider::builder()
    .with_api_token(&api_token)
    .with_category("outages", true)
    .build()?;
```

### Full BGP security bootstrap

```rust
let bootstrap = CloudflareRadarBootstrapProvider::builder()
    .with_api_token(&api_token)
    .with_category("bgp_hijacks", true)
    .with_category("bgp_leaks", true)
    .with_bgp_asn_filter(vec![13335, 16509])
    .with_hijack_min_confidence(80)
    .with_event_date_range("30d".into())
    .build()?;
```

### Wiring bootstrap → source → query

```rust
use drasi_bootstrap_cloudflare_radar::CloudflareRadarBootstrapProvider;
use drasi_source_cloudflare_radar::CloudflareRadarSource;

// 1. Build bootstrap
let bootstrap = CloudflareRadarBootstrapProvider::builder()
    .with_api_token(&token)
    .with_category("outages", true)
    .with_category("domain_rankings", true)
    .with_ranking_limit(50)
    .build()?;

// 2. Build source with the same categories
let source = CloudflareRadarSource::builder("radar")
    .with_api_token(token)
    .with_category("outages", true)
    .with_category("domain_rankings", true)
    .with_ranking_limit(50)
    .with_bootstrap_provider(bootstrap)   // ← bootstrap attached here
    .build()?;

// 3. Register source with QueryHost (see example app for full wiring)
```

## Key Behaviors

- **Selective loading** — Only elements matching the query's required node labels and relationship types are returned, keeping the initial load focused.
- **Retry with backoff** — Transient Cloudflare API errors (429, 5xx, network failures) are retried up to 4 times with exponential backoff.
- **Filter parity** — The bootstrapper supports the same location, ASN, confidence, and domain filters as the source.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `Cloudflare Radar API token is required` | Empty or missing token | Set `with_api_token` |
| `DNS category enabled but no dns_domains provided` | `dns` on without domains | Provide `with_dns_domains(vec![...])` |
| `At least one category must be enabled` | All categories disabled | Enable ≥ 1 category |
| Empty bootstrap results | Filters too restrictive or wrong date range | Widen filters or increase `event_date_range` |
| Query results empty after startup | Bootstrap categories don't match query labels | Align bootstrap categories with the query's `MATCH` labels |
