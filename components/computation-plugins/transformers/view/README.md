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

## Configuration

The required fields are `stream`, `middleware` and `pipeline`. The stream names
this view's output. Middleware names must be unique, and every pipeline step
must name a declared middleware. An empty pipeline is an identity view.

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

The implementation reuses the existing **map** and **relabel** processors. A map
receives a complete current input image as an `Insert`, including after a
replacement or patch. Therefore configure only its `insert` rules. An explicit
rule `op` may be `Insert` or `Replace`; both mean a complete projected image.
Event-specific `update` / `delete` rules and output `Update` / `Delete`
operations are rejected at construction, not silently ignored. Every pipeline
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

This component does not add a mode to, or change the behavior of, the existing
stateless map/jq middleware or native middleware transformer. Jq and other
middleware kinds are not registered by this view factory.

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

Outputs preserve input provenance and lineage through the shared codec, but
have their own **volatile** producer identity and sequence. One output envelope
is produced per accepted input envelope, including an empty progress-only
envelope when the view did not change. Retractions use the recorded output
identity and labels, with the triggering input's effective time.

## Storage, snapshots and lifecycle

Storage is **memory-backed only**. It retains current full inputs, current
outputs and their associations, not an ever-growing event log. Filtered inputs
still consume memory; deleting them releases their retained state. Memory usage
depends on the current dataset and projection fan-out, with no configured
byte/count limit.

`out` exposes a coherent full-image snapshot and its matching producer epoch
and logical sequence while the component is running. Readers can capture the
previous committed snapshot during a pending transform. Captured snapshots
remain immutable across later updates. A snapshot is the view of inputs
observed by this instance; it does not fetch or certify an upstream dataset.
There is no snapshot-input/bootstrap capability. Seed the instance with complete
input images before sending patches for them.

Stopping and restarting the **same component object** preserves its state and
epoch, without re-running mappings. Reconstruction creates an empty view and a
fresh volatile epoch; rebuild it from full inputs. Snapshot reads before start
or while stopped return `NotReady`; readers of a removed/dropped instance return
`Retired`. Explicit deprovision after stop (or before first start) clears retained
inputs and outputs and permanently fences the instance. It is idempotent and owns no
background work or external resources.

The advertised capabilities are output snapshots, an originating output
progress boundary and explicit deprovision. There is **no durable storage,
replay, snapshot-follow, transaction participation, input deduplication,
scheduled processing, durable bootstrap, or atomic state/output/delivery
recovery**. In-memory staging is not a storage transaction or a delivery
acknowledgement. A downstream delivery/transport failure cannot be recovered
from an outbox here. Use ordered, lossless input delivery for a faithful view;
this component cannot reconstruct missing changes.
