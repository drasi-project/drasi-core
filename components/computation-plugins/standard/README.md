# Standard native ComputationGraph plugin

`drasi-computation-standard` supplies real graph components through native ABI
**1.0.0**, independently of the unchanged Source/Reaction/Bootstrap ABI **0.15.0**.
The package uses the workspace version; that version is not its ABI version.
Plugin identity is `drasi-computation-standard`, implementation versions are `"1"`,
and configuration versions are `1`.

| Implementation | Role | Configuration |
|---|---|---|
| `drasi.standard/volatile-counter` | `EnvelopeSource` | `stream` required; `count=10`, `start=0`, `step=1`, `interval_ms=0`, `paused=false` |
| `drasi.standard/middleware` | `Transformer` | Required `stream`, `middleware`, `pipeline`; supports existing `map` and `relabel` middleware |
| `drasi.standard/capture` | `EnvelopeSink` | `path` required; `append=false` |
| `drasi.standard/arithmetic` | `Transformer`, optional transaction participant | `stream` required; `field="value"`, `add=0`, `multiply=1`, `counter_property="batch_count"` |

Every data port uses `GraphChangeCodec::schema()`. Sources expose `out`, sinks
expose `in`, and transformers expose both. Configuration getters return the
complete normalized configuration, including defaults, never transient counters.
Construction performs validation only; file/timer work begins in host-owned
lifecycle/data operations.

## Volatile example pipeline

The counter emits a finite sequence of inserted `Counter` nodes. Each has an
integer `value`, an element ID derived from its emission number, and the configured
output stream. It rejects negative counts and ranges overflowing `i64`.

**The counter is a volatile example, not a persistent or replayable source.**
Reconstruction resets its position. Its `drasi.standard.volatile-counter`
annotation documents this but is not a recovery checkpoint or a persistent
producer identity. Do not use it to feed durable consumers that require
restart-stable input. Durable tests instead supply exact host-owned input
identities/sequences.

The counter accepts dedicated control notifications:

```text
Custom { kind: "drasi.counter.pause",  payload: null }
Custom { kind: "drasi.counter.resume", payload: null }
```

Pause gates the next emission before its interval wait; it does not roll back an
in-flight operation. Control handling remains independent of a blocked `next()`.
Ordinary peer status notifications are advisory. Unknown custom commands and
non-null payloads fail explicitly.

The middleware factory reuses `MiddlewareTransformer`, not a legacy Source or
Reaction wrapper. For example:

```json
{
  "stream": "middleware/out",
  "middleware": [{
    "name": "rename",
    "kind": "relabel",
    "config": {"labelMappings": {"Counter": "Projected"}}
  }],
  "pipeline": ["rename"]
}
```

The selected middleware implementations are stateless. The surrounding transformer
owns volatile sequencing/element bookkeeping and marks its producer identity as
nonpersistent. No jq feature or bundled jq build is required.

The capture sink writes one complete `EnvelopeCodec` JSON frame per line to its
configured file. `append=false` truncates on start; `append=true` preserves prior
contents. Successful handling means the write and flush completed, **not** fsync
durability, transactional external effects or exactly-once delivery. Cancellation
or an I/O failure can leave a partial line. Retained transaction output can be
redelivered and will be written again; a durable application sink must implement
its own appropriate idempotency/acceptance contract.

## Transactional arithmetic

Arithmetic transforms integer properties as `(value + add) * multiply`, with
checked overflow and explicit type errors. Standalone execution is volatile and
requires complete numeric values on updates.

Opted-in execution uses the existing host `TransactionTransformer`: each step
borrows a native transaction state capability. It stores `batches`, `last_value`
and previous raw elements only in that step's transaction namespace. Patches can
merge with previous raw element state; deletes remove that state. Each output
includes the configured batch-count property. Distinct steps can use the same
state keys without sharing state. No commit method, workers or external effects
exist in the participant.

For two steps with `add=2,multiply=1,counter_property="first_count"` and
`add=0,multiply=4,counter_property="second_count"`, input `value=3` produces
`value=20,first_count=1,second_count=1`. The container owns atomic state/output
commit and recovery. A retry of committed input may redeliver retained output
without rerunning either step.

The component supports serialized state `get/put/remove`, element
`get/put/remove`, and host-authoritative `derive` through the native C operations.
The Rust `TransactionContext`, trait objects, futures, `Arc` and `Bytes` never
cross the library boundary. Cancelling a step revokes its borrowed RPC capability;
the host remains responsible for draining already-submitted storage I/O and
rolling back its transaction.

## Build and execute the separate-library tests

From the `drasi-core` workspace:

```sh
mkdir -p target/native-standard-validation
export TMPDIR="$PWD/target/native-standard-validation"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3
cargo build --offline -p drasi-computation-standard --features dynamic-plugin
cargo test --offline -p drasi-host-sdk --test native_computation_pipeline
```

The host test intentionally does **not** depend on this Rust crate. It loads
`target/debug/libdrasi_computation_standard.dylib` on macOS, the corresponding
`.so` on Unix or `.dll` on Windows, using public `computation::load`.
`DRASI_NATIVE_STANDARD_PLUGIN` can select another build artifact. A missing
binary is an explicit failure with the build prerequisite, never a skipped test.

The tests also exercise the shared family loader's path and directory discovery,
metadata-only scan, family-qualified registry identity, category/duplicate checks,
and graph/transaction factory registries using the actual native library.
They cover exact graph outputs, a mixed native/host graph, configuration
and topology reconstruction, lifecycle and codec rejection, independent control
while data is pending, cancellation, and real RocksDB-backed transaction state
isolation, error rollback, cancellation rollback and committed replay. A complete
durable graph additionally roundtrips host-owned RocksDB/retained-store recipes
and reconstructs its native participants and sink.

Native libraries are process-pinned. Query-role snapshot/outbox hosting, source
recovery-progress handles, native resource injection, in-place reconfiguration
and hot unloading remain explicitly unsupported. Host-owned transaction/index/
retained-pipe resources do not extend those native ABI capabilities.
