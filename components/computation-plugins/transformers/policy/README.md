# Native Rego policy transformer

`drasi-computation-transformer-policy` provides `drasi.standard/policy`
(implementation `"1"`, configuration version `1`). It evaluates user-supplied
[Rego](https://www.openpolicyagent.org/docs/latest/policy-language/) policies
with the embedded [regorus](https://github.com/microsoft/regorus) engine
(`=0.12.0`). It evaluates once for each current input element and maintains one
decision record per input. Both `in` and `out` use the standard
`GraphChangeCodec`.

```sh
cargo build -p drasi-computation-transformer-policy --features dynamic-plugin
```

Load the resulting native library through the Host SDK, or use `plugin()` /
`PolicyFactory` for native SDK registration. Nothing is fetched at runtime:
policies, data and the entrypoint are all configuration.

## Configuration

The required fields are `stream`, `modules` and `query`. Unknown fields are
rejected. The policy is compiled when the configuration is validated, including
the entrypoint. Invalid Rego, an undefined entrypoint rule, or an over-limit
module or data document therefore rejects the configuration before the
component exists.

| Field | Default | Meaning |
|---|---|---|
| `stream` | required | Output stream name. |
| `modules` | required | Non-empty list of `{ "name", "source" }` Rego modules. Names must be unique and non-empty; they appear in compile errors. |
| `data` | `{}` | Static JSON object exposed as `data` alongside the policy packages. |
| `query` | required | Entrypoint rule path, starting with `data.` (for example `data.gpu.placement.decision`). |
| `rego_version` | `"v1"` | `"v1"` or `"v0"` Rego syntax. |
| `labels` | `[]` | Input labels to evaluate. Empty means every element. Other elements are neither retained nor evaluated. |
| `input` | `"$properties"` | Template that builds the Rego `input` from the element document (see below). |
| `output_label` | `"PolicyDecision"` | Label of every decision node. |
| `output_properties` | `{"decision": "$decision"}` | Map from output property name to a template over the result document. |
| `copy_input_properties` | `false` | Copy the input element's properties onto the decision before `output_properties` is applied. |
| `provenance` | `true` | Add `policy_hash` and `policy_input_fingerprint`. |
| `on_error` | `"emit"` | `"emit"` records an error decision; `"fail"` rejects the whole input batch. |
| `strict_builtin_errors` | `false` | Make builtin errors (for example division by zero) evaluation errors instead of undefined. |
| `timeout_ms` | `100` | Evaluation time limit, 1–60000. |
| `timeout_check_interval` | `64` | Instructions between time checks, 1–1000000. |
| `max_input_bytes` | 1 MiB | Maximum canonical JSON size of one `input`, up to 64 MiB. |
| `max_decision_bytes` | 1 MiB | Maximum JSON size of one decision value, up to 64 MiB. |
| `max_retained_inputs` | 100000 | Maximum retained input elements, up to 10000000. |
| `max_policy_bytes` | 1 MiB | Maximum total module source size, up to 16 MiB. |
| `max_data_bytes` | 16 MiB | Maximum canonical `data` size, up to 256 MiB. |

### Templates

Templates are JSON values. Objects and arrays are walked recursively; strings
beginning with `$` are selectors and every other value is a literal. Use `$$`
to start a literal string with `$`. A selector is a root followed by
dot-separated keys or array indexes, for example `$properties.cluster.region`
or `$decision.reasons.0`.

- `input` roots are `$id`, `$namespace`, `$kind` (`"node"` or `"relation"`),
  `$labels`, `$properties`, `$in_node` and `$out_node` (relations only, each
  `{namespace, id}`). `$` alone selects that whole element document.
- `output_properties` roots are `$decision`, `$input`, `$element` (the element
  document above), `$status` and `$error`.

If a selected value is missing, an object member is omitted, an array element
becomes `null` and a top-level property is omitted. If the whole `input`
template is missing, the record is an evaluation error. Date-time properties
are passed to Rego as strings.

Output property names must be non-empty. They must not use the reserved names
`policy_status`, `policy_error`, `policy_hash` or `policy_input_fingerprint`.

### Example

This runs gpu-cluster-lab's unchanged `placement.rego` over rows from an
upstream continuous query that labels each workload/cluster pair
`PlacementPair`:

```json
{
  "stream": "placement-eligibility",
  "modules": [{ "name": "placement.rego", "source": "package gpu.placement\nimport rego.v1\n..." }],
  "query": "data.gpu.placement.decision",
  "labels": ["PlacementPair"],
  "input": {
    "workload": {
      "workload_id": "$properties.workload_id",
      "data_profile_id": "$properties.data_profile_id",
      "purpose": "$properties.purpose"
    },
    "data_profile": "$properties.data_profile",
    "policy": "$properties.policy",
    "cluster": "$properties.cluster"
  },
  "output_label": "PlacementEligibility",
  "output_properties": {
    "workload_id": "$input.workload.workload_id",
    "cluster_id": "$input.cluster.cluster_id",
    "allowed": "$decision.allow",
    "reasons": "$decision.reasons"
  },
  "timeout_ms": 50
}
```

### Inputs from a continuous query

The transformer has no joins: when a decision depends on several kinds of
record (for example a workload, a deployment and the rules that link them),
an upstream continuous query should produce one row per thing to decide, with
everything the policy reads, as in the example above. Set the query's
`result.label` and list that label in `labels`. Each row's result-node
identity becomes its decision's identity, so put a readable key in a column
and copy it with `output_properties` (for example `"id": "$element.properties.id"`).

Keep each row's content to what the policy reads. A change to any column
changes `policy_input_fingerprint` and re-emits the decision, even if the
outcome is the same. Two query behaviours matter here:

- `collect()` does not keep a stable order, so a collected list can change
  order without changing content. Prefer per-row values, or collect only
  values that cannot repeat in a different order (at most one per row).
- An aggregating query keeps a group after all of its matched elements are
  deleted, recomputed over an empty set. To retract the row, and so its
  decision, add `count(...) AS matches` in a `WITH` and filter
  `WHERE matches > 0` before `RETURN`.

## Decisions

Each retained input has exactly one decision node. The node has the input's
`ElementReference`, the label `output_label` and the input's effective time.
Its properties are:

- the copied input properties, when enabled;
- `output_properties`;
- `policy_status`: `"decided"`, `"undefined"` (the entrypoint produced no
  value) or `"error"`;
- `policy_error`, only for errors (runtime failures, timeouts,
  size limits, missing input);
- when `provenance` is enabled:
  - `policy_hash`: `sha256:` over the canonical Rego version, modules, query
    and data;
  - `policy_input_fingerprint`: `sha256:` over the canonical `input` JSON.

Rego sets are returned as sorted arrays, and object keys are canonicalized.
Equal inputs and policies therefore always produce identical decisions.

Inserts, replacements and patches (`ElementPatch` over the retained image)
re-evaluate the affected input from its final image in the batch. A changed
decision is emitted as an insert or replacement; an unchanged decision emits
nothing. Deleting an input, or relabelling it out of `labels`, retracts its
decision. A patch for an unretained element that matches `labels` is rejected;
one that does not match is ignored. `Future` operations are rejected. A
staging failure, `on_error: "fail"` evaluation error or retention-limit breach
rejects the whole batch without advancing state or the output sequence. One
output envelope is produced per accepted input envelope, including an empty
progress-only envelope.

### Determinism and limits

Policies cannot read the clock, network or randomness. `time.now_ns`,
`rand.intn`, `uuid.rfc4122`, `http.send`, `opa.runtime` and
`net.lookup_ip_addr` are replaced by extensions that always fail evaluation,
whatever `strict_builtin_errors` is set to. Engine `print` output is captured and discarded.

`timeout_ms` uses regorus' cooperative execution timer. It checks every
`timeout_check_interval` instructions. It bounds runaway policies, but it is
wall-clock time. A policy that finishes close to the limit can therefore succeed
or time out depending on machine load. Choose a generous bound. There is no
memory limit: regorus' allocator-based limits require a global allocator, which
a plugin must not install. Use the byte limits and `max_retained_inputs` to
bound memory instead.

## Managed updates

The component supports update planning and in-place apply. Changing `stream`
or `labels`, or lowering `max_retained_inputs` below the live input count,
requires `Replace`. Every other change, including modules, data, query,
templates, output label and limits, is applied at a `Pause` boundary:

1. The proposed policy is compiled.
2. Every retained input is re-evaluated against it, with cooperative yields
   and cancellation.
3. Only decisions that differ from the published ones are queued.

If any re-evaluation fails under `on_error: "fail"`, or apply is cancelled,
the update is refused and the live configuration is unchanged. After resume,
queued changes are emitted through continuations in batches of 1024. Their
effective time is no earlier than the latest committed input. New input for a
queued element supersedes its queued change.

## Storage, snapshots and lifecycle

State is **memory-backed and volatile**. The component retains the current full
inputs and published decisions. `out` exposes a coherent snapshot of the
current decisions while running. Before start or while stopped it returns
`NotReady`, and after erase it returns `Retired`. Pause refuses input until
resume. Stop then start on the same object preserves state. Disposal with erase
(only while stopped) clears state and permanently fences the instance.
Reconstruction starts empty with a new volatile epoch. There is no durable
storage, replay or snapshot-follow capability.

### Initializing from upstream snapshots

A continuous query does not send its bootstrap results downstream, so a policy
fed by a query sees only rows that change after it starts. To start from the
current rows, bind the factory's `output_snapshots` dependency (a
`nativeOutputSnapshots` resource listing the outputs connected to `in`). The
factory declares `binds_inputs` and supports `output-snapshots-v1`. With the
dependency bound:

- At its first start the transformer reads each incoming edge's snapshot.
  It then queues a decision for every current input as continuation output.
- A live envelope that the snapshot already covers is skipped: same producer
  epoch, sequence at or below the snapshot's. It still produces its (empty)
  output envelope.
- An upstream that restarts (a new producer epoch) is read again. Elements it
  no longer has are deleted, so their decisions are retracted.
- Stop then start keeps the positions and reads nothing again.

Envelopes without producer progress, or from a producer that is not a bound
edge, are applied as they are. Without the dependency nothing is read, and
the transformer behaves as described above. The SDK's
`UpstreamSnapshots` helper implements this for any volatile transformer.
