# Cloudflare Radar — Graph Schema Reference

This document is the authoritative reference for the graph schema produced by the Cloudflare Radar source and bootstrap provider. Every node, relationship, and property is documented here with its type, source API field, and ID format.

---

## Node Types

### Outage

Internet outage event detected by Cloudflare Radar.

| Property | Type | API Field | Description |
|----------|------|-----------|-------------|
| `dataSource` | String | `dataSource` | Data source identifier |
| `description` | String | `description` | Human-readable outage description |
| `scope` | String | `scope` | Geographic or network scope of the outage |
| `startDate` | String (ISO 8601) | `startDate` | When the outage began |
| `endDate` | String (ISO 8601) | `endDate` | When the outage ended (`null` if ongoing) |
| `eventType` | String | `eventType` | Type of event |
| `linkedUrl` | String | `linkedUrl` | URL to more information |
| `outageCause` | String | `outage.outageCause` | Root cause (e.g. "Cable Cut", "Government Shutdown") |
| `outageType` | String | `outage.outageType` | Outage classification |

**Node ID format:** User-defined outage identifier (normalized from API)
**Effective from:** Parsed from `startDate`, falls back to current time

---

### Location

ISO 3166-1 alpha-2 country/region. Shared across outages.

| Property | Type | Description |
|----------|------|-------------|
| `code` | String | Two-letter ISO country code (e.g. `"US"`, `"DE"`) |

**Node ID format:** `location-{code}` (e.g. `location-US`)

---

### AutonomousSystem

An autonomous system identified by its ASN. Shared across outages, hijacks, and leaks.

| Property | Type | Description |
|----------|------|-------------|
| `asn` | Integer | Autonomous System Number |

**Node ID format:** `as-{asn}` (e.g. `as-13335`)

---

### BgpHijack

A BGP route hijack event.

| Property | Type | API Field | Description |
|----------|------|-----------|-------------|
| `eventId` | Integer | `id` | Unique event identifier |
| `duration` | Integer | `duration` | Duration in seconds |
| `confidenceScore` | Integer | `confidence_score` | Detection confidence (0–100) |
| `minHijackTs` | String (ISO 8601) | `min_hijack_ts` | Earliest hijack timestamp |
| `maxHijackTs` | String (ISO 8601) | `max_hijack_ts` | Latest hijack timestamp |
| `hijackMsgCount` | Integer | `hijack_msgs_count` | Number of hijack BGP messages seen |
| `peerIpCount` | Integer | `peer_ip_count` | Number of peer IPs that observed the hijack |
| `isStale` | Boolean | `is_stale` | Whether the event is considered stale |

**Node ID format:** `bgp-hijack-{id}` (e.g. `bgp-hijack-42`)
**Effective from:** Parsed from `min_hijack_ts`

---

### BgpLeak

A BGP route leak event.

| Property | Type | API Field | Description |
|----------|------|-----------|-------------|
| `eventId` | Integer | `id` | Unique event identifier |
| `leakCount` | Integer | `leak_count` | Number of leaked routes |
| `leakType` | Integer | `leak_type` | Leak classification type |
| `detectedTs` | String (ISO 8601) | `detected_ts` | When the leak was detected |
| `minTs` | String (ISO 8601) | `min_ts` | Earliest leaked route timestamp |
| `maxTs` | String (ISO 8601) | `max_ts` | Latest leaked route timestamp |
| `originCount` | Integer | `origin_count` | Number of origin ASNs affected |
| `peerCount` | Integer | `peer_count` | Number of peers that saw the leak |
| `prefixCount` | Integer | `prefix_count` | Number of leaked prefixes |
| `finished` | Boolean | `finished` | Whether the leak event has ended |

**Node ID format:** `bgp-leak-{id}` (e.g. `bgp-leak-77`)
**Effective from:** Parsed from `detected_ts`

---

### Prefix

An IP address prefix (CIDR notation). Shared across hijack events.

| Property | Type | Description |
|----------|------|-------------|
| `prefix` | String | CIDR prefix (e.g. `"1.0.0.0/24"`) |

**Node ID format:** `prefix-{normalized}` (e.g. `prefix-1_0_0_0_24`)

Non-alphanumeric characters in the prefix are replaced with `_`.

---

### HttpTraffic

HTTP traffic device-type distribution summary for a time series point.

| Property | Type | Description |
|----------|------|-------------|
| `series` | String | Time series label (e.g. `"summary"`) |
| `desktop` | String | Desktop traffic percentage |
| `mobile` | String | Mobile traffic percentage |
| `other` | String | Other device traffic percentage |

**Node ID format:** Defined by the source (e.g. `http-traffic-summary`)

---

### AttackL7

Layer 7 (application layer) attack distribution summary.

| Property | Type | Description |
|----------|------|-------------|
| `series` | String | Time series label |
| `ddos` | String | DDoS attack percentage |
| `waf` | String | WAF-mitigated percentage |
| `ipReputation` | String | IP reputation-based percentage |
| `accessRules` | String | Access rules percentage |
| `botManagement` | String | Bot management percentage |
| `apiShield` | String | API Shield percentage |
| `dataLossPrevention` | String | DLP percentage |

**Node ID format:** Defined by the source (e.g. `attack-l7-summary`)

---

### AttackL3

Layer 3 (network layer) attack protocol distribution summary.

| Property | Type | Description |
|----------|------|-------------|
| `series` | String | Time series label |
| `udp` | String | UDP attack percentage |
| `tcp` | String | TCP attack percentage |
| `icmp` | String | ICMP attack percentage |
| `gre` | String | GRE attack percentage |

**Node ID format:** Defined by the source (e.g. `attack-l3-summary`)

---

### Domain

A tracked internet domain.

| Property | Type | Description |
|----------|------|-------------|
| `name` | String | Fully qualified domain name (e.g. `"google.com"`) |

**Node ID format:** `domain-{normalized}` (e.g. `domain-google_com`)

---

### DomainRanking

The current global rank for a domain.

| Property | Type | Description |
|----------|------|-------------|
| `rank` | Integer | Global ranking position (1 = most popular) |
| `domain` | String | The domain this ranking applies to |

**Node ID format:** `ranking-{normalized_domain}` (e.g. `ranking-google_com`)

---

### DnsQuerySummary

DNS query analytics summary for a specific domain.

| Property | Type | Description |
|----------|------|-------------|
| `domain` | String | The queried domain |
| `topLocations` | JSON Array | Top locations by query volume, each entry has `clientCountryAlpha2`, `clientCountryName`, `value` |

**Node ID format:** `dns-summary-{normalized_domain}`

---

## Relationship Types

### AFFECTS_LOCATION

An outage affects a geographic location.

```
(Outage)-[:AFFECTS_LOCATION]->(Location)
```

| Property | Type | Description |
|----------|------|-------------|
| *(none)* | — | No properties on this relationship |

**Relationship ID format:** `{outage_id}-affects-location-{code}`

---

### AFFECTS_ASN

An outage affects an autonomous system.

```
(Outage)-[:AFFECTS_ASN]->(AutonomousSystem)
```

| Property | Type | Description |
|----------|------|-------------|
| *(none)* | — | No properties on this relationship |

**Relationship ID format:** `{outage_id}-affects-as-{asn}`

---

### HIJACKED_BY

A BGP hijack was perpetrated by a specific autonomous system.

```
(BgpHijack)-[:HIJACKED_BY]->(AutonomousSystem)
```

| Property | Type | Description |
|----------|------|-------------|
| *(none)* | — | No properties on this relationship |

**Relationship ID format:** `bgp-hijack-{id}-hijacker-as-{asn}`

---

### VICTIM_ASN

A BGP hijack targeted a victim autonomous system.

```
(BgpHijack)-[:VICTIM_ASN]->(AutonomousSystem)
```

| Property | Type | Description |
|----------|------|-------------|
| *(none)* | — | No properties on this relationship |

**Relationship ID format:** `bgp-hijack-{id}-victim-as-{asn}`

---

### TARGETS_PREFIX

A BGP hijack targeted a specific IP prefix.

```
(BgpHijack)-[:TARGETS_PREFIX]->(Prefix)
```

| Property | Type | Description |
|----------|------|-------------|
| *(none)* | — | No properties on this relationship |

**Relationship ID format:** `bgp-hijack-{id}-targets-prefix-{normalized_prefix}`

---

### LEAKED_BY

A BGP leak originated from a specific autonomous system.

```
(BgpLeak)-[:LEAKED_BY]->(AutonomousSystem)
```

| Property | Type | Description |
|----------|------|-------------|
| *(none)* | — | No properties on this relationship |

**Relationship ID format:** `bgp-leak-{id}-leaked-as-{asn}`

---

### RANKED_AS

A domain has a specific global ranking.

```
(Domain)-[:RANKED_AS]->(DomainRanking)
```

| Property | Type | Description |
|----------|------|-------------|
| *(none)* | — | No properties on this relationship |

**Relationship ID format:** `domain-{normalized}-ranked-as-ranking-{normalized}`

---

## Visual Schema

```
                            ┌──────────┐
                        ┌──>│ Location │
                        │   └──────────┘
  ┌────────┐  AFFECTS_  │
  │ Outage │──LOCATION──┘
  │        │──AFFECTS_ASN──┐
  └────────┘               │   ┌───────────────────┐
                           └──>│ AutonomousSystem  │
                           ┌──>│                   │
  ┌───────────┐ HIJACKED_ │   └───────────────────┘
  │ BgpHijack │──BY───────┘           ▲
  │           │──VICTIM_ASN───────────┘
  │           │──TARGETS_PREFIX──┐
  └───────────┘                  │   ┌────────┐
                                 └──>│ Prefix │
                                     └────────┘
  ┌──────────┐  LEAKED_BY
  │ BgpLeak  │──────────────────────>│ AutonomousSystem │
  └──────────┘

  ┌────────┐  RANKED_AS   ┌───────────────┐
  │ Domain │──────────────>│ DomainRanking │
  └────────┘               └───────────────┘

  ┌─────────────┐   ┌───────────┐   ┌───────────┐
  │ HttpTraffic │   │ AttackL7  │   │ AttackL3  │
  └─────────────┘   └───────────┘   └───────────┘

  ┌─────────────────┐
  │ DnsQuerySummary │
  └─────────────────┘
```

## ID Normalization

All element IDs pass through `normalize_id()` which replaces every non-alphanumeric character with `_`. For example:

| Input | Normalized |
|-------|-----------|
| `1.0.0.0/24` | `1_0_0_0_24` |
| `google.com` | `google_com` |
| `US` | `US` |

## Categories → Node Types

| Category | Node Types Produced | Relationship Types Produced |
|----------|--------------------|-----------------------------|
| `outages` | Outage, Location, AutonomousSystem | AFFECTS_LOCATION, AFFECTS_ASN |
| `bgp_hijacks` | BgpHijack, AutonomousSystem, Prefix | HIJACKED_BY, VICTIM_ASN, TARGETS_PREFIX |
| `bgp_leaks` | BgpLeak, AutonomousSystem | LEAKED_BY |
| `http_traffic` | HttpTraffic | *(none)* |
| `attacks_l7` | AttackL7 | *(none)* |
| `attacks_l3` | AttackL3 | *(none)* |
| `domain_rankings` | Domain, DomainRanking | RANKED_AS |
| `dns` | DnsQuerySummary | *(none)* |
