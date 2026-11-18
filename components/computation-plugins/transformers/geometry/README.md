# Native planar geometry transformer

`drasi-computation-transformer-geometry` provides `drasi.standard/geometry`
(implementation `"1"`, configuration version `1`). It reads planar geometries
from graph-change records and groups them into named **sets**. For every pair
of members that satisfies a configured spatial **relation**, it maintains one
relation record. Both `in` and `out` use the standard `GraphChangeCodec`, so any
source or query can feed it.

```sh
cargo build -p drasi-computation-transformer-geometry --features dynamic-plugin
```

Load the resulting native library through the Host SDK, or use `plugin()` /
`GeometryFactory` for native SDK registration. It depends only on the
computation SDK, the contracts and [`geo`](https://docs.rs/geo).

## Configuration

```json
{
  "stream": "00000000-0000-4000-8000-000000000002/out",
  "sets": [
    {"name": "journeys", "label": "JourneyFootprint", "key": "journey_id",
     "geometry": {"type": "lineString", "property": "points"}},
    {"name": "obstacles", "label": "ObstacleFootprint", "key": "obstacle_id",
     "geometry": {"type": "polygon", "property": "vertices"}}
  ],
  "relations": [{
    "name": "obstruction", "left": "journeys", "right": "obstacles",
    "predicate": "withinDistance",
    "threshold": {"left": ["radius", "clearance"]},
    "label": "Obstruction",
    "properties": {"id": "id", "leftKey": "journey_id", "rightKey": "obstacle_id",
                   "distance": "distance_m", "threshold": "required_m"},
    "copyLeft": {"cart_id": "cart_id"}
  }]
}
```

| Field | Default | Meaning |
| --- | --- | --- |
| `stream` | required | The output stream. It must equal the stream the host binds to this component's `out` port (Drasi Server: `<component id>/out`); otherwise the graph rejects the emission. It cannot be changed by an update. |
| `sets` | required | Named roles (at least one). Every set must be used by a relation. |
| `relations` | required | Relations between two sets (at least one). |
| `epsilon` | `1e-7` | Non-negative length tolerance. It is used for distance comparisons, ring closure, zero-length edges and degenerate rings. |
| `ringClosure` | `auto` | `auto` accepts open or closed rings. `required` requires each ring to repeat its first coordinate, as GeoJSON does. |
| `invalidGeometry` | `fail` | `fail`, `skip` or `report` (see below). |
| `errorLabel` | `GeometryError` | The label of error records under `report`. |
| `limits` | `{maxElements: 10000, maxVertices: 4096, maxOutputs: 100000}` | Resource bounds. Each must be between 1 and 10,000,000. |

### Sets

`{name, label, where?, key?, geometry}`

- An input element belongs to a set when it carries `label` and every `where`
  property equals the given JSON value (numbers compare numerically). An
  element can belong to several sets. Elements in no set are not retained.
- `key` names a property holding the member's identity: a nonempty string or an
  integer. Without `key`, the identity is the input element id. Keys must be
  unique within a set. A duplicate is an invalid record, and it is not retried
  when the original holder leaves until it changes again.
- `geometry` says how to read the shape:
  - `{type: point, property}`: the property holds `[x, y]` or `{x, y}`.
  - `{type: point, x, y}`: two numeric properties hold the coordinates.
  - `{type: lineString, property}`: an array of at least two coordinates.
  - `{type: polygon, property}`: one ring of coordinates, or an array of rings
    (the exterior first, then the holes).
  - `{type: geoJson, property}`: a GeoJSON `Point`, `MultiPoint`, `LineString`,
    `MultiLineString`, `Polygon` or `MultiPolygon`, or a `Feature` holding one.
    Extra ordinates beyond x and y are ignored.

  Any property may also hold the same value as JSON text.

A geometry is invalid if any of these hold:
- a coordinate is non-numeric or non-finite;
- an edge is shorter than `epsilon`;
- a line string has fewer than 2 points, or a ring has fewer than 3 distinct
  vertices;
- a ring encloses no area (its mean width is at most `epsilon`), or is open
  when closure is required;
- it fails `geo`'s validity checks (for example, a self-intersecting ring or a
  hole outside its exterior);
- it has more than `maxVertices` vertices.

### Relations

`{name, left, right, predicate, threshold?, label, properties?, copyLeft?, copyRight?}`

| `predicate` | Holds when |
| --- | --- |
| `withinDistance` | distance(left, right) ≤ threshold + `epsilon`. Touching counts. |
| `intersects` | the geometries share any point. |
| `contains` | left contains right (DE-9IM). |
| `within` | left is within right (DE-9IM). |

`threshold` is required by `withinDistance`, and only accepted there. It is
`{constant?, left?: [property…], right?: [property…]}`. The threshold for a
pair is `constant` plus the sum of the named left-member properties plus the
sum of the named right-member properties. Each named property must be a
finite, non-negative number; otherwise the member is invalid. For example,
`{left: ["radius", "clearance"]}` reproduces "path within radius + clearance
of the obstacle".

A relation can pair a set with itself. Pairs are ordered, and a member is
never paired with itself.

Each pair that satisfies the relation becomes one node with the relation's
`label`:
- Its element id is the JSON text `["relation", name, leftKey, rightKey]`, so
  it is stable for as long as the pair satisfies the relation.
- `properties` names the generated properties. A name of `null` or an omitted
  name leaves that property out:
  - `id` (default `"id"`): `"<leftKey>/<rightKey>"`.
  - `relation`: the relation name.
  - `leftKey` and `rightKey`: the member keys.
  - `distance`: the Euclidean distance, as a float.
  - `threshold`: the evaluated threshold, as a float (`withinDistance` only).
- `copyLeft` and `copyRight` map output property names to member property
  names. Missing source properties are omitted.
- Every output property name must be unique.

## Semantics

- **Incremental evaluation.** Each input batch changes retained members. Only
  pairs involving a changed member are re-evaluated. A bounding-box prefilter
  skips exact distance and topology tests for distant pairs. The output batch
  holds deletes first, then inserts and replaces. A record whose label and
  properties are unchanged is not re-emitted. Output is ordered by relation
  and member key, so it is deterministic regardless of input order.
- **Patches.** `Update` changes are merged onto the retained full image. A patch
  for a set member with no retained image is an error. A patch for an element
  outside every set is ignored. Deleting an unknown element is ignored.
- **Invalid records.** Invalid geometry, keys and threshold properties are
  handled per `invalidGeometry`:
  - `fail` rejects the whole batch and leaves the state unchanged.
  - `skip` excludes the record from the set and retracts its relations.
  - `report` also emits an `errorLabel` node with id `["error", set, namespace,
    elementId]` and properties `set`, `element` and `error`. The node is
    retracted when the record is fixed or removed.
- **Bounds.** A batch that would retain more than `maxElements` set members,
  or produce more than `maxOutputs` records, fails with the state unchanged.
  Elements outside every set do not count. Pair evaluation is a linear scan
  over the other set, bounded by these limits. An R-tree index (`rstar`) is
  intentionally not used, because it would add a dependency.
- **Inputs.** Only graph data is accepted. Control, scheduled (`Future`) and
  query snapshot/progress recovery frames are rejected.
- **Output and recovery.** The output is volatile. Each envelope carries
  producer progress at the transformer's own sequence. The `out` port
  publishes a snapshot of the current records while the transformer is
  running.
- **Lifecycle.** Input is refused while paused. Stop and Start retain state. A
  Preserve dispose keeps state. An erasing dispose, which requires a stopped
  transformer, drops the state and retires the snapshot.
- **Updates.**
  - Changing `stream` is unsupported.
  - Changing `limits`, or switching `invalidGeometry` between `fail` and
    `skip`, is applied in place at a Pause (state preserved). If the retained
    state already exceeds the new limits, the update is assessed as Replace
    instead.
  - Every other change, including changed input bindings, needs Replace with a
    reset. A transformer can only emit in response to input, so it cannot emit
    the retractions a semantic change implies. Its state is rebuilt from the
    replayed input instead.

## Example: move-a-wall obstructions

The configuration above reproduces the move-a-wall demo's geometry component.
Two upstream continuous queries flatten the scene into footprint rows, and the
transformer's output feeds the existing `obstructions` query (`MATCH
(o:Obstruction)`) unchanged.

```yaml
# Journey ⋈ Cart, with result label JourneyFootprint
joins:
  - id: USES
    keys:
      - {label: Journey, property: cart_id}
      - {label: Cart, property: id}
query: |
  MATCH (j:Journey)-[:USES]->(c:Cart)
  WHERE j.active AND c.active
  RETURN j.id AS journey_id, c.id AS cart_id, j.points AS points,
         c.radius AS radius, c.clearance AS clearance

# Active obstacles, with result label ObstacleFootprint
query: |
  MATCH (o:Obstacle) WHERE o.active
  RETURN o.id AS obstacle_id, o.vertices AS vertices
```

Deactivating a journey, cart or obstacle removes its footprint row, which
retracts its obstructions. The tests replay the demo's fixture and wall sweep,
including the tangent and just-clear positions. They compare each step with
the demo's own calculation.
