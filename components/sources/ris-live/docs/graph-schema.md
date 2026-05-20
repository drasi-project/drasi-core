# RIS Live Graph Schema Reference

This document describes the graph schema produced by the RIS Live source. All BGP data from RIS Live is mapped into a labeled property graph of **Peer** nodes, **Prefix** nodes, and **ROUTES** relationships.

## Visual Overview

```
┌─────────────────────────┐             ┌──────────────────────┐
│       (:Peer)           │             │     (:Prefix)        │
│─────────────────────────│   ROUTES    │──────────────────────│
│ peer_ip   : String      │───────────▶│ prefix : String      │
│ peer_asn  : String      │             └──────────────────────┘
│ host      : String      │
│ state     : String?     │   The ROUTES relationship carries:
│ timestamp : Float?      │     next_hop, origin_asn, path,
│ msg_id    : String?     │     path_length, origin, community,
└─────────────────────────┘     peer, peer_asn, host, timestamp
```

A single RIS Live connection to one route collector (e.g. `rrc00`) will produce one `Peer` node per BGP session peer, one `Prefix` node per distinct IP prefix, and one `ROUTES` edge per unique peer–prefix combination currently announced.

---

## Nodes

### Peer

Represents a BGP speaker connected to a RIS route collector.

| Field | Description |
|---|---|
| **Label** | `Peer` |
| **ID format** | `{host}\|{peer_ip}` (e.g. `rrc00.ripe.net\|208.80.153.193`) |

**Properties:**

| Property | Type | Always present | Description |
|---|---|---|---|
| `peer_ip` | `String` | ✅ | IP address of the BGP peer |
| `peer_asn` | `String` | ✅ | Autonomous System Number of the peer |
| `host` | `String` | ✅ | RIS route collector hostname (e.g. `rrc00.ripe.net`) |
| `state` | `String` | ❌ | BGP session state from `RIS_PEER_STATE` messages (e.g. `connected`, `down`) |
| `timestamp` | `Float` | ❌ | Unix timestamp of the last message from this peer |
| `msg_id` | `String` | ❌ | RIS message identifier |

**Lifecycle:**

| BGP Event | Effect |
|---|---|
| First announcement from this peer | **INSERT** new Peer node |
| Subsequent announcements | No change (Peer already known) |
| `RIS_PEER_STATE` (new peer) | **INSERT** new Peer node with `state` |
| `RIS_PEER_STATE` (known peer) | **UPDATE** Peer node's `state` property |

**Notes:**
- The Peer node ID includes the collector hostname, so the same physical BGP peer observed by different collectors produces separate Peer nodes.
- `peer_asn` is stored as a String because some RIS Live messages provide it that way.

---

### Prefix

Represents an IP prefix (IPv4 or IPv6 CIDR block) advertised by one or more peers.

| Field | Description |
|---|---|
| **Label** | `Prefix` |
| **ID format** | `{prefix}` (e.g. `203.0.113.0/24` or `2001:db8::/32`) |

**Properties:**

| Property | Type | Always present | Description |
|---|---|---|---|
| `prefix` | `String` | ✅ | The IP prefix in CIDR notation |

**Lifecycle:**

| BGP Event | Effect |
|---|---|
| First announcement of this prefix | **INSERT** new Prefix node |
| Subsequent announcements | No change (Prefix already known) |

**Notes:**
- Prefix nodes are never deleted. Withdrawals remove the `ROUTES` relationship but leave the Prefix node in place. This is intentional: the prefix remains a valid entity in the graph even when temporarily unrouted.
- Both IPv4 (e.g. `192.0.2.0/24`) and IPv6 (e.g. `2001:db8::/32`) prefixes are handled identically.

---

## Relationships

### ROUTES

Represents an active BGP route from a peer to a prefix. The relationship direction is:

```
(Peer)-[:ROUTES]->(Prefix)
```

This reads as "Peer **routes** traffic toward Prefix".

| Field | Description |
|---|---|
| **Label** | `ROUTES` |
| **ID format** | `{host}\|{peer_ip}\|{prefix}` (e.g. `rrc00.ripe.net\|208.80.153.193\|203.0.113.0/24`) |
| **Start node** | `Peer` (in_node) |
| **End node** | `Prefix` (out_node) |

**Properties:**

| Property | Type | Always present | Description |
|---|---|---|---|
| `next_hop` | `String` | ✅ | IP address of the next-hop router |
| `prefix` | `String` | ✅ | The advertised prefix (duplicated from Prefix node for convenience) |
| `peer` | `String` | ✅ | IP of the announcing peer (duplicated from Peer node) |
| `peer_asn` | `String` | ✅ | ASN of the announcing peer |
| `host` | `String` | ✅ | Route collector hostname |
| `origin` | `String` | ❌ | BGP origin attribute (`IGP`, `EGP`, or `INCOMPLETE`) |
| `path` | `String` (JSON) | ❌ | Full AS path as JSON array. Each element is either an integer (ASN) or an array of integers (AS_SET). Example: `[14907,3356,64500]` |
| `path_length` | `Integer` | ❌ | Number of AS hops in the path (AS_SET members are counted individually) |
| `origin_asn` | `Integer` | ❌ | The originating AS number (last ASN in the path) |
| `community` | `String` (JSON) | ❌ | BGP community attributes as JSON array of `[asn, value]` pairs. Example: `[[14907,100],[3356,3]]` |
| `timestamp` | `Float` | ❌ | Unix timestamp of the BGP message |
| `msg_id` | `String` | ❌ | RIS message identifier |

**Lifecycle:**

| BGP Event | Effect |
|---|---|
| Announcement (new peer+prefix combo) | **INSERT** new ROUTES relationship |
| Announcement (existing peer+prefix) | **UPDATE** ROUTES properties (e.g. path change) |
| Withdrawal | **DELETE** ROUTES relationship |

**Notes:**
- The relationship ID includes `{host}|{peer_ip}|{prefix}`, so the same physical peer announcing the same prefix will produce distinct ROUTES edges when observed by different collectors.
- When a route is updated (e.g. AS path changes), all properties are replaced with the new announcement's values.
- Withdrawals only delete the ROUTES edge. The Peer and Prefix nodes remain.

---

## Derived Properties

Some ROUTES properties are computed from the raw BGP message:

| Property | Derived from | Logic |
|---|---|---|
| `path_length` | `path` | Count of ASNs in the path. Each ASN in an AS_SET counts individually. |
| `origin_asn` | `path` | Last ASN in the AS path (walking backwards, taking the last element of the last segment). |

---

## ID Formats Summary

| Element | ID Format | Example |
|---|---|---|
| Peer node | `{host}\|{peer_ip}` | `rrc00.ripe.net\|208.80.153.193` |
| Prefix node | `{prefix}` | `203.0.113.0/24` |
| ROUTES rel | `{host}\|{peer_ip}\|{prefix}` | `rrc00.ripe.net\|208.80.153.193\|203.0.113.0/24` |

---

## State Transitions

The graph starts empty and fills as announcements arrive. Over time:

```
t=0   (empty graph)

t=1   Announcement: host=rrc00, peer=10.0.0.1, prefix=192.168.0.0/16
      → INSERT Peer "rrc00|10.0.0.1"
      → INSERT Prefix "192.168.0.0/16"
      → INSERT ROUTES "rrc00|10.0.0.1|192.168.0.0/16"

t=2   Announcement: host=rrc00, peer=10.0.0.1, prefix=10.0.0.0/8
      → INSERT Prefix "10.0.0.0/8"                              (Peer already known → no insert)
      → INSERT ROUTES "rrc00|10.0.0.1|10.0.0.0/8"

t=3   Announcement: host=rrc00, peer=10.0.0.1, prefix=192.168.0.0/16  (path changed)
      → UPDATE ROUTES "rrc00|10.0.0.1|192.168.0.0/16"           (properties refreshed)

t=4   Withdrawal: host=rrc00, peer=10.0.0.1, prefix=10.0.0.0/8
      → DELETE ROUTES "rrc00|10.0.0.1|10.0.0.0/8"               (Peer and Prefix nodes remain)

t=5   RIS_PEER_STATE: peer=10.0.0.1, state=down
      → UPDATE Peer "rrc00|10.0.0.1"  (state → "down")
```

---

## Example Query Patterns

### All routes from a specific peer

```cypher
MATCH (peer:Peer)-[r:ROUTES]->(p:Prefix)
WHERE peer.peer_ip = '208.80.153.193'
RETURN p.prefix AS prefix,
       r.next_hop AS next_hop,
       r.path_length AS hops
```

### Prefixes with unusually long paths

```cypher
MATCH (peer:Peer)-[r:ROUTES]->(p:Prefix)
WHERE r.path_length > 6
RETURN p.prefix AS prefix,
       r.origin_asn AS origin,
       r.path_length AS hops,
       peer.peer_asn AS observer
```

### Routes originated by a specific AS

```cypher
MATCH (peer:Peer)-[r:ROUTES]->(p:Prefix)
WHERE r.origin_asn = 13335
RETURN p.prefix AS prefix,
       peer.peer_ip AS seen_by,
       r.path_length AS hops,
       r.next_hop AS next_hop
```

### Peer session states

```cypher
MATCH (peer:Peer)
WHERE peer.state IS NOT NULL
RETURN peer.peer_ip AS peer,
       peer.peer_asn AS asn,
       peer.host AS collector,
       peer.state AS session_state
```

### Count routes per peer

```cypher
MATCH (peer:Peer)-[r:ROUTES]->(p:Prefix)
RETURN peer.peer_ip AS peer,
       peer.peer_asn AS asn,
       count(r) AS route_count
```
