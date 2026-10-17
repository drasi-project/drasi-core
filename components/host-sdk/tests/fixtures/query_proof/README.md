# Query-component performance investigation

This branch preserves the initial experiment from Core
`30c5df88a8047f904dd2f2abe2991465074a843a`. The investigation is separate from the
working `agentofreality-parallel-computation-graph` branch. Its purpose is to
identify the cost of making the query component a plugin, reduce that cost
where feasible, and document what the measurements actually establish.

This is a test-only real dynamic-library proof, not a production Query factory.
Production query execution still uses the existing built-in implementation.
Do not complete or deploy the larger optional-plugin migration as an assumed
part of this investigation.

## Preserved starting point

- `src/computation/query_proof.rs` (relative to the Host SDK package) is the
  host-side driver, resource handler, correctness checks and measurement.
- `../native_query_proof.rs` is the test library's entry point.
- `client.rs`, `engine.rs` and `wire.rs` implement the plugin-side resource
  client, shared evaluator setup and test-only protocol.
- `macos-baseline.json` contains the original interleaved measurements and
  binary hashes. Its `remote` flag means "across the dynamic-library boundary,"
  not a different computer or a network connection.
- `baseline-cargo-lock.txt` preserves the dependency resolution used locally.
  Root `Cargo.lock` is not tracked in this repository.
- `transfer-provenance.json` records the exact captured file hashes and the
  toolchain reported from the original Core checkout.
- `lib/docs/computation-graph-query-plugin-plan.md`, relative to the repository
  root, contains the broader design and the implementation decisions.

The original Core checkout used Rust 1.95.0. The coordinator directory can
select a different Rust version, so do not infer the benchmark toolchain from
a shell outside the Core checkout.

## Reproduce in the isolated checkout

Use a target directory owned by this checkout, not the original local
workspace's shared target. Use the preserved lockfile as this experiment's
`Cargo.lock`; compare and record any pre-existing lockfile before replacing it.
Record the actual toolchain, target, enabled features, host resources and binary
hashes for each new result.

From the repository root, after preparing that lockfile:

```sh
export CARGO_TARGET_DIR="$PWD/target/query-component-perf"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=1
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_PROFILE_RELEASE_STRIP=none

cargo +1.95.0 build --locked --release \
  -p drasi-host-sdk --example native_query_proof
cargo +1.95.0 test --locked --release -p drasi-host-sdk --lib \
  computation::query_proof::dynamic_query_resource_proof_matches \
  -- --test-threads=1
cargo +1.95.0 test --locked --release -p drasi-host-sdk --lib \
  computation::query_proof::dynamic_query_resource_proof_measurement \
  -- --exact --ignored --nocapture --test-threads=1
```

The measurement is intentionally opt-in. It alternates direct and dynamic
execution order over five repetitions per workload/path. Each repetition has
256 warmup inputs followed by 2,000 timed inputs. Both paths use the same
evaluator, host transaction ownership and input/result encoding. The host
consumes the foreign result buffer without an additional copy to a local Vec.

The two correctness cases compare projection and aggregation results and row
identities against direct execution on current-thread and multi-thread Tokio
runtimes, with independent expected-value checks.

## Initial observation, not a causal conclusion

The median of the five per-run average input times was:

| Workload | Direct | Dynamic | Dynamic/direct time |
|---|---|---|---|
| Projection | 13.209 microseconds | 128.448 microseconds | 9.724 times |
| Sum aggregation | 15.016 microseconds | 254.150 microseconds | 16.925 times |

The dynamic path made seven and fifteen resource calls per input, respectively.
This measures the combined cost of the current resource-boundary design.
It does not isolate a library call from serialization, allocation, mailbox
traffic, wakeups, polling and index access. It does not show that dynamic
plugins inherently impose those multipliers.

On a remote Linux host, compare direct and dynamic execution on that same host.
Do not treat differences from the macOS timings as improvements or regressions
without controlling for the platform, toolchain and machine.

## Investigation boundaries

Preserve correctness checks and the existing query engine. Attribute costs with
matched controls and measurements before claiming a cause. Any diagnostic that
changes data locality, transaction ownership or supported behavior must be
labelled as such; it is not automatically a valid replacement design.

The proof currently bounds scans at 128 elements and messages at 1 MiB, and
rejects virtual-join setup. It has not qualified the complete RocksDB/shared
journal, bootstrap, scheduled-work, recovery, API or deployment path. Do not
weaken those requirements to label the full plugin migration complete.

The user approved low-level constructor migration to factory-based creation and
manual updates of old saved native definitions, but those changes have not
been made. No production performance regression has been accepted. Stop for a
decision before adopting a larger ownership/architecture change or accepting
a regression in the production path.

Keep all experiments in this isolated branch. Do not update or merge into the
working computation-graph branch, modify the other local checkouts, or run
benchmarks on the user's local machine from a cloud session.
