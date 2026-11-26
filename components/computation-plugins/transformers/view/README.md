# Native stateful view transformer

`drasi-computation-transformer-view` provides `drasi.standard/view`
(implementation `"1"`, configuration version `1`). It maintains a transformed
dataset, rather than mapping each incoming event independently. Both `in` and
`out` use the standard `GraphChangeCodec`.

```sh
cargo build -p drasi-computation-transformer-view --features dynamic-plugin
```

Load the resulting native library through the Host SDK, or use `plugin()` /
`ViewFactory` for native SDK registration. It has no dependency on the query
evaluator, graph runtime or legacy plugin SDK.

The view is a stateful transformer. It runs on the same runtime as the
[query transformer](../query) and the
[stateful middleware](../middleware/README.md#stateful-middleware), described
in the [native transformer guide](../NATIVE-TRANSFORMER-GUIDE.md#stateful-transformers),
and requires host-owned processing.

## Configuration

The required fields are `stream`, `middleware` and `pipeline`, with the same
meaning and the same declared kinds (`decoder`, `map`, `parse_json`, `promote`,
`relabel`) as the [middleware transformer](../middleware): both use the one
pipeline in [`drasi-computation-middleware-common`](../../common/middleware).
The stream names this view's output. Middleware names must be unique, and every
pipeline step must name a declared middleware. An empty pipeline is an identity
view.

This example projects the active children of each `Parent` node:

```json
{
  "stream": "active-children",
  "middleware": [
    {
      "name": "children",
      "kind": "map",
      "config": {
        "Parent": {
          "insert": [
            {
              "condition": "$[?(@.active == true)]",
              "selector": "$.children[*]",
              "id": "$['$selected'].id",
              "label": "Child",
              "properties": {
                "name": "$['$selected'].name",
                "parent": "$.parentId"
              }
            }
          ]
        }
      }
    }
  ],
  "pipeline": ["children"]
}
```

Every step receives a complete current input image as an `Insert`, including
after a replacement or patch, and must return complete images. Therefore a map
declares only `insert` rules. An explicit rule `op` may be `Insert` or
`Replace`; both mean a complete projected image. This restriction is part of
the published configuration schema: event-specific `update` / `delete` rules
and output `Update` / `Delete` operations are rejected when the graph binds
the view (and again at construction), not silently ignored. Every pipeline
step operates on complete images. The view determines the actual output change
operations by comparing its previous and newly projected datasets.

Map rules retain their existing selectors, conditions, labels, property
expressions and node/relation `elementType` configuration. For example, a rule
can create a relationship from properties of an input node:

```json
{
  "id": "$.relationshipId",
  "label": "HAS_CHILD",
  "elementType": {
    "Relation": {
      "inNodeId": "$.parentId",
      "outNodeId": "$.childId"
    }
  },
  "properties": {
    "weight": "$.weight"
  }
}
```

An unconfigured label or unmatched map selection contributes no output, but its
full input image is still retained for subsequent patches. Mapping identity is
the shared `ElementReference`, including its source namespace, not just the
element key. Distinct inputs must not produce the same output identity; duplicate
identities within one input's projection are also errors. The component does
not invent merging or ownership rules for ambiguous outputs.

The view and the stateless middleware transformer share one pipeline; the view
only adds the complete-image restriction above. `jq` (system libjq) and
`unwind` (stored images) are not declared by this factory.

The view also accepts the runtime's shared options:

| Option | Effect |
| --- | --- |
| `bootstrap` | Initialize from the bound bootstrap provider or the connected input's current state |
| `volatile` | Keep state in memory; every initialization publishes a new output epoch |
| `publication` | `atomic` (default) or `non_atomic` |
| `recovery` | `strict` (default) or `auto_reset` |
| `resetConfiguration` | Reset retained state when the configuration changed |
| `outboxCapacity` | Committed outputs retained for replay and restart delivery (default 1) |

Retained state is reused only with the same `middleware` and `pipeline`.

## Dataset semantics

- **Add and replace:** retain the complete input, dropping omitted old
  properties. Either operation can establish an absent input or replace an
  existing one.
- **Patch:** apply the shared `ElementPatch` to the retained full input. Omitted
  properties remain; explicit removals delete properties; `null` remains a
  value. Metadata and relationship endpoints come from the patch. A patch
  without a retained base, a kind-changing patch or a conflicting set/remove
  operation fails. Missing values are never inferred from the partial image.
- **Delete:** remove the input and retract its actual recorded outputs.
  Deleting an absent input is a no-op. No mapping is evaluated for a deleted
  input.

The view retains the actual outputs emitted for each input. A filter becoming
false retracts those outputs; a shrinking selection deletes removed children;
an output-key change deletes the old identity before adding the new one.
Existing output identities receive full replacements, so properties omitted
from the new projection cannot linger downstream. Changing an output between
node and relationship emits a delete followed by an add. Relationship endpoint
changes are full replacements. There is no implicit cascading deletion of
unrelated relationships.

Within an input envelope, operations are applied in their specified order, then
each surviving affected input is mapped once from its final complete image.
The output is the net dataset change for that envelope. All affected old
ownership is released before new identities are checked, so two inputs can
exchange keys in the same envelope. A mapping error, unknown patch base or
collision rejects the whole staged batch without advancing state or output
sequence. Cancellation before publication also discards the staged batch.
Old images are never re-evaluated to discover their historical output, including
if a future mapping implementation is nondeterministic.

Outputs preserve input provenance and lineage through the shared codec, under
the view's own producer identity, epoch and sequence. An input envelope that
leaves the view unchanged advances its committed input progress but publishes
nothing and does not advance the output sequence. Retractions use the recorded
output identity and labels, with the triggering input's effective time.

## Storage, bootstrap and recovery

The view retains each current full input in the host element index, the
outputs each input produced in a private table, and its output rows and
history through the runtime. It does not keep an event log. Filtered inputs are
still retained; deleting an input releases its state. Storage grows with the
current dataset and projection fan-out, with no configured byte or count limit.

With a persistent processing provider this state survives restart and
reconstruction. The view replays unconfirmed output and continues from its
committed input position. It does not re-run mappings to recover outputs it
already emitted, because each input's outputs are recorded. With `volatile`
or an in-memory provider, state is held in memory and each initialization
publishes a new output epoch.

With `bootstrap`, the view initializes from its bound bootstrap provider or
from the connected input's current state, so later patches apply to the
bootstrapped images. Without it, seed the view with complete input images
before sending patches for them.

`out` supports current-state, snapshot-and-follow and replay reads. A current
state is the view of the inputs this instance observed; it does not fetch or
certify an upstream dataset. After a gap in its input's retained history,
`auto_reset` with a connected input rebuilds the view from that input's
current state and publishes only the keyed difference, so a running dependent
follows the correction without a reset. A dependent that installed the empty
view before the first input connected receives the first input's rows the same
way. A rebuild that would strand a running dependent is refused. A crash during
a rebuild either keeps the old view and retries, or completes the installed
rebuild and publishes the owed difference. Deleting the component's owned
resources erases its retained state.
