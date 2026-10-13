# Normal DrasiLib instance host

`drasi-instance-host` is a reusable hosting library and thin executable. Every
instance, including a parent, child or grandchild, embeds the **same normal
DrasiLib**. There is no child evaluator, remote transformer, `next` RPC, or
cross-process Rust resource interface.

The portless native service factory `drasi.host/local-child`, implementation
version `"1"` and configuration version `1`, owns a **local process**. Native
`drasi.bridge/source` and `drasi.bridge/sink` factories are registered separately
and carry data. Supervision relationships do not define the data-flow graph.

## Configuration and persistent identity

A minimal manifest is:

```json
{
  "version": 1,
  "instance_id": "orders",
  "state_dir": "/absolute/private/orders",
  "plugins": []
}
```

Run it with:

```sh
drasi-instance-host --config /absolute/path/orders.json
```

The application supplies absolute, normalized paths. The state directory must
be owner-only (`0700`), must not traverse symlinks, and must be short enough for
a Unix socket (`state_dir/c.sock`, at most 100 bytes). A process-exclusive file
lock prevents overlapping instance owners. `identity.json` permanently binds
the directory to `instance_id`; a new generation never deletes or resets it.

Additional fields:

| Field | Meaning |
|---|---|
| `desired` | Normal `drasi_lib::management::DesiredInstance`: native topology, resource recipes, lifecycle policies and recovery assertions. |
| `plugins` | Exact native-library allowlist: `{"path":"/absolute/plugin.so","sha256":null}`. An optional SHA-256 pin is checked before loading. No directory discovery. |
| `configuration_store` | Optional `{"key_file":"/absolute/private/key"}`. Uses existing encrypted redb management, at `state_dir/configuration.redb`. Key file is owner-only, exactly 32 raw bytes, and must survive restart. |
| `indexes` | Named providers, e.g. `{"local":{"kind":"rocksdb"}}`. Existing RocksDB computation providers use distinct `state_dir/indexes/<name>` namespaces. |
| `runtime` | Host executor, child count, control framing, diagnostics, deadline and shared RocksDB budgets described below. |
| `stop_stages` | Explicit ordered composition cuts, described below. |

The normal Host SDK `PluginRegistry`, `HostManagementResources` recipes,
standard native factories and public DrasiLib management APIs remain the
authorities. Supported middleware recipes register `map` and `relabel`; this is
not a second component registry. Configuration persistence is **opt-in**.
Without redb, `desired` is the startup seed on each new host. With redb, the
seed initializes only an absent record: accepted state, even an accepted empty
definition, wins over an obsolete seed.

Host/child seed manifests are privileged, owner-only JSON, not encrypted
processing-state backups. Prefer unresolved secret references instead of
literal credentials in seed manifests. Accepted redb definitions use the existing
encrypted store. Processing state, configuration state and host observations are
different things.

### Bridge control and TLS resources

The executable wraps the existing SDK resolver with public `InstanceResources`.
These are normal graph-owned resource recipes; `binding` must equal the resource
ID and no resource dependencies are accepted by these three recipes:

| Recipe | Resource role | Component dependency |
|---|---|---|
| `{"kind":"bridgeIngressControl"}` | `Component` | Source `control` |
| `{"kind":"bridgeEgressControl"}` | `Component` | Sink `control` |
| `bridgeTls` | `Identity` | Source/sink `tls` |

Each control resource belongs to one endpoint. The resolver retains only a weak
reference to the actual graph-owned control, not another component registry.
Ordinary SDK recipes and their recovery guards are forwarded. In-place changes
to accepted control/TLS recipes are rejected rather than bypassing those guards;
retirement/reconstruction remains subject to normal management eligibility.
Use one resolver per instance/graph.

TLS construction delegates to the bridge package's shared
`TlsResourceRecipe::resolve(base)`, with this host's state directory as `base`.
There is no second certificate parser or plaintext fallback. For example:

```json
{
  "kind": "bridgeTls",
  "version": 1,
  "role": "client",
  "trust_roots": ["certs/root.der"],
  "certificate_chain": ["certs/client.der"],
  "private_key": "certs/client.pk8",
  "peer_certificate_sha256": "<64 lowercase hexadecimal characters>"
}
```

A source requires `server` credentials; a sink requires `client` credentials.
The shared helper enforces bounded DER roots/chains, PKCS8 keys, path validation,
private-key permissions and peer pinning. Key material is loaded only when the
owning instance constructs the resource. Durable ingress additionally uses the
existing `QosChannel` **`StateStore`** resource in `admission`; durable egress uses
the existing `QueryIndexProviderResource` in `state`. See the
[bridge contract](../computation-plugins/bridge/README.md) for those recipes.

### Declaring an owned child

Create a normal native component specification with:

```rust,ignore
let child = HostConfig::empty("child", "/private/instances/child");
let specification = SupervisorFactory::specification(
    ComponentId::try_new("child-owner")?,
    &child,
)?;
```

Put that factory specification into a `DesiredComponent` with role `Service`,
no ports/streams, and ordinary lifecycle policy. Its sole configuration key is
`manifest`, containing the child's complete `HostConfig`. Nested manifests are
bounded and checked for distinct instance/state identities. Construction only
reserves a supervisor slot; the normal `start` hook launches the process.

The child receives its own manifest and loads **only its own** allowlist.
Parent validation never opens child libraries, discovers child factory
metadata, or invokes child executable schema validators. All transaction and
shared-storage resources stay inside their owning instance. Matching names,
paths or serialized recipes cannot transfer a transaction domain.

`factories()` exports only the supervisor factory, never bridge factories.
For registries that require pure static registration, grant a process-local
`ChildSupervisorResource::handle(supervisor)` with resource role `Component`
and declare it in the component's `owner` dependency. The capability chooses
the executable, namespaces and budgets outside the manifest. The generic CLI
registers the same factory with `SupervisorFactory::new(supervisor)`, binding
its already owned supervisor. An unbound factory rejects missing `owner`
capabilities; registration alone does not grant a Server process-launch
authority. The resource can be supplied by the embedding application's normal
resource resolver; it is not a serialized Rust handle or a new core runtime.

The generic executable supplies its own executable path when launching children,
without a shell. Library embedders explicitly supply their host executable to
`ChildSupervisor::new` / `run`; manifests cannot choose arbitrary commands.

## Control and lifecycle

`control.json` is an owner-only local credential record containing a fresh
random token and process-generation UUID. It is not a data-plane credential.
Control uses a separate Unix socket, 4-byte big-endian lengths, bounded JSON,
version negotiation, exact instance/generation/request correlation, and
authentication before every operation. Allocation is bounded before frame
decoding; the JSON recursion limit remains enabled.

`ControlClient` exposes synchronous and asynchronous requests. Use
`request_async` from Tokio. Operations include:

- version/capability handshake and live status;
- privileged accepted configuration and revision;
- ordinary desired-state acceptance and reconciliation;
- explicit local start and component quiesce;
- composition stop, stop/repair/force of an exactly identified owned child;
- operation-result lookup; executable-only force exit.

Mutating requests enter a bounded, generation-local idempotency ledger before
execution. Reusing a request ID with changed content fails. Accepted operations
survive a disconnected/cancelled caller. A deadline records **incomplete** while
retaining the actual operation future; it does not cancel a commit, release
ownership, or start overlapping work. The ledger does not silently evict old
requests and reinterpret them as new work. When full, new mutations are
rejected; status and executable force exit remain available. Persistent
configuration receipts separately use the normal DrasiLib management contract.

Readiness uses the currently accepted configuration, not an obsolete startup
seed. Required auto-start components must be created and started under the
graph's declared readiness contract, without known failure/degradation. An
explicit readiness-confirmation component must actually report ready; an
independent health value of `Unknown` is not itself failed startup. Internal
inspection nodes and intentionally stopped components do not invent readiness
requirements. This is not inferred from a PID, successful spawn or listener.
Startup errors remain observable through control.
The supervisor verifies host version/capabilities, instance ID, generation,
manifest fingerprint and health. A cached child observation is explicitly
marked stale when unavailable; unreachable does not mean exited.

One independent supervisor thread owns each instance's child handles. Ownership
is reserved and persisted **before** launch. Cancellation, readiness failure,
stop timeout and force requests retain the handle until `wait`/`try_wait` proves
exit and reaps it. Diagnostics are bounded. Definitions and last-known
observations remain on disk after failure.

Terminal crashes have no automatic restart policy. Repair is explicit and
requires the exact old, reaped generation. Successful stop persists stopped
intent, including across parent reconstruction. A changed host process UUID
does not change instance identity, graph namespace, state directory, or bridge
stream identity. Old-generation control requests cannot operate a replacement.
Updating a child manifest does not silently clear a previous stopped intent;
starting a new process after that stop requires explicit repair/start intent.
After abrupt owner loss, a newly started parent has **lost ownership**, not a
reaped handle. Explicit repair verifies absence of the old PID and exclusive
access to the stable child state directory before launch. This is recorded
separately as `exit_verified`; `reaped` is not fabricated. A reused/still-live
PID conservatively prevents repair and is never signalled without a retained
`Child` handle.

## Composition shutdown is not `DrasiLib::stop`

The host offers `stop_tree` as a **hosting protocol over normal local APIs**.
It never adds a secret core shutdown mode or infers global drain from empty
local queues.

For a graph without bridges, the default is child-first graceful stop/reap,
then ordinary local `DrasiLib::shutdown`.

For inter-instance data, ordered cuts can be described and validated:

```json
{
  "stop_stages": [
    {"kind":"quiesce","components":["external-input","outgoing-transform","to-child"]},
    {"kind":"stop_child","component":"child-owner"},
    {"kind":"quiesce","components":["from-child","result-consumer"]}
  ]
}
```

Quiesce uses public `GraphControl::quiesce_components` at processing boundaries.
Select the complete local upstream path that must drain, not just a queue
counter. A bridge egress must have an explicit cut before stopping its owned
receiver. Return ingress remains active until its owned sender has stopped.
Each child uses the same protocol recursively. Control stays alive throughout.

After an egress and its complete upstream path quiesce,
`BridgeEgressControl::finish` rejects unresolved sender obligations and seals
the actual peer ingress against the sender's resolved receipt. Only then does
the supervisor request ordinary child `StopTree`.

Before quiescing an ingress, its host requires an already sealed,
generation-bound `IngressCut` from `BridgeIngressControl::sealed_cut` and
awaits `wait_drained`. Volatile evidence means every admitted envelope was
returned by the source; the subsequent normal graph quiescence finishes its
in-flight forward and downstream processing. Durable evidence checks the
actual required journal subscriber cursors. A child config therefore includes
its own ingress-to-egress quiesce stage. Its return egress finishes while the
parent's return source and control lane are still active.

Final checks revalidate the inlet cuts, reject pending/unsealed egress, and
quiesce the normal graph before `DrasiLib::shutdown`. Successful shutdown
observations retain the exact cuts in `stop_cuts`. A partial cleanup retries
the same library shutdown without reopening admissions or resuming processing.
No process-control message contains a remote component invocation or a
fabricated data-plane receipt.

This proves only the configured local processing/bridge **admission** boundaries,
not cross-instance atomic completion or external-effect rollback. A missing
cut, unfinished external producer, partition, unhealthy instance, pending native
stop or unresolved receipt returns an incomplete/error outcome and retains
ownership. In particular, the generic host refuses to claim an independently
owned remote producer is quiesced without an actual transport-owned sealed cut.
Reference/remote peers are never implicitly launched, replaced or terminated.

Force is separate, explicit and scoped to an entire owned instance process
group. It can resolve process cleanup while leaving prior data/commit outcomes
unknown. It is not component-level preemption or a successful drain.

## Parent death and platform limits

The current implementation supports Unix owned-local hosting (Linux/macOS).
It explicitly rejects unsupported platforms; this is not a Windows named-pipe
implementation or remote-machine provisioner.

Every executable has its own process group. An owned child inherits only the
read end of a dedicated parent lifetime pipe. **Before loading any plugin** it
starts a dedicated OS watchdog thread which blocks on that pipe. EOF or an
invalid ownership message terminates that instance's process group. Parent
writers are close-on-exec and are not inherited by grandchildren. Each
grandchild has the same independent owner pipe, so abrupt parent loss cascades
through the owned hierarchy. Native noncooperative graph work cannot starve
this watchdog through Tokio scheduling.

This is a process-owner watchdog, **not a security sandbox or a kernel-wide
hard process-tree fence**. A whole process suspended by the OS cannot schedule
its watchdog until resumed; arbitrary native code can defeat a cooperative
host's file descriptors or process groups. The policy assumes trusted native
plugins and a scheduling OS. Children in uninterruptible kernel work may take
time to exit after `SIGKILL`; the owner retains unresolved cleanup rather than
claiming reaping. Direct children are reaped by their supervisor. On abrupt
owner death, the platform's orphan adopter is responsible for reaping them.
`require_kernel_parent_death` explicitly rejects a requested hard kernel
guarantee rather than presenting this watchdog as one.

Dropping an embedded hosting future requests emergency termination through the
independent child owner; only an awaited successful graceful stop reports
graceful completion. Normal executable exit closes lifetime pipes too.

### Default budgets

- 2 graph workers, 4 Tokio blocking workers per host;
- 8 direct children (maximum configurable: 32), hierarchy depth at most 8;
- 8 concurrent control connections, approximately 1 MiB control frames;
- 128 retained control operations; no unbounded request history;
- 4 KiB retained diagnostics per child;
- 30-second startup/operation budgets, 500-ms peer round trips,
  250-ms health polling;
- 64 MiB shared RocksDB cache/write-buffer budget across this host's registered
  index providers.

Runtime worker budgets are enforced by the executable. Library `run` users
must supply a runtime honoring them. Plugin-owned runtimes, native allocations,
process RSS, CPU affinity and security containment are not hard-limited by
these settings. A requested `require_hard_resource_isolation` is rejected,
never silently downgraded to scheduling budgets.

Large hierarchical status responses explicitly set `observation_truncated`
and omit detailed component/descendant observations instead of exceeding the
control frame budget. Top-level health is computed from all local observed
components before truncation. Use each instance's own authenticated endpoint
to resynchronize details; missing detail is not evidence of health.

## Validation

From the repository root, using the existing shared Cargo target directory:

```sh
mkdir -p target/instance-host-build
export TMPDIR="$PWD/target/instance-host-build"
export CARGO_BUILD_JOBS=3 CARGO_INCREMENTAL=0
cargo build --locked --offline -p drasi-instance-host --features dynamic-plugin --example instance_host_fixture
cargo build --locked --offline -p drasi-computation-standard --features dynamic-plugin
cargo test --locked --offline -p drasi-instance-host --lib --test processes -- --test-threads=1
```

The fixture is a **real separately loaded native SDK library**, not a shell
process or a fake data bridge. A missing library is a test failure with the
build prerequisite. `DRASI_INSTANCE_HOST_FIXTURE` may select a built fixture.
Tests launch the actual same host binary in separate processes and cover:
hierarchical identity and namespaces; child-only native code loading; actual
initialization failure/health; disconnected control callers; retained timed-out
native cleanup; explicit force/reaping; crash repair and generation fencing;
and abrupt parent death with genuinely non-yielding native grandchild work.
An additional same-executable two-process test sends real graph changes through
separate native bridge nodes and a native arithmetic library, stops during
traffic, and compares the exact returned results against the actual sealed
frontier before verifying clean process exit. A separate test rejects an empty
but unsealed ingress, proves its listener remains usable, then supplies a real
native seal and retries shutdown.

Test scratch directories are private repository-local `.ih-*` directories,
short enough for macOS Unix sockets, removed by the test owner. No tests use a
global temporary directory. Every wait has a watchdog and every test process
has a scoped cleanup guard.

### Qualified Unix platforms and prepared network peer

The 13 October 2026 checkpoint passed the 7 unit and 10 local process cases on
macOS and Linux, with separately rebuilt Host/native libraries. Linux containers
must have an init/reaper (`podman run --init`): parent-death tests require actual
grandchild reaping, not merely a zombie with a signal recorded. The qualification
used Rust 1.95 on Bookworm, the checkout mounted read-only at `/workspace`, a
writable Cargo target at `/build`, and the cached Cargo registry. Real RocksDB
test fixtures also require `libclang-dev`; install build prerequisites in the
owned qualification container, not on another user's host or container.

The additional `network::durable_tls_cross_os_faults_reopen_and_independent_peer_ownership`
case requires a prepared Linux peer with the current Host, standard native
library and fixture built under `/build/debug`. Publish its data/control ports
and use the same source checkout as the macOS caller. For the qualified local
VM arrangement, run from the macOS repository root:

```sh
DRASI_NETWORK_CONTAINER=cg-qualification \
DRASI_NETWORK_DATA_ADDRESS=127.0.0.1:35431 \
DRASI_NETWORK_CONTROL_ADDRESS=127.0.0.1:35432 \
DRASI_NETWORK_BIND_DATA_ADDRESS=0.0.0.0:35431 \
DRASI_NETWORK_BIND_CONTROL_ADDRESS=0.0.0.0:35432 \
CARGO_BUILD_JOBS=3 cargo test --locked --offline -p drasi-instance-host \
  --lib --test processes -- --include-ignored --test-threads=1
```

`DRASI_NETWORK_CONTAINER` names the prepared, init-enabled Podman container.
The address variables distinguish the caller-visible published ports from
the peer's bind addresses. Selecting the network case without those prerequisites
fails; it does not silently pass or downgrade to loopback. The runtime-parity
runner includes it when the peer is explicitly configured. Its unprepared-peer
exception has an owner and expiry in `lib/tests/runtime_parity/allowed-ignored.tsv`.

This is actual macOS-to-Linux-VM networking through the normal mutual-TLS bridge,
with real receipt loss/delay, version/authentication rejection, exact outputs,
RocksDB/encrypted-redb reopen and independent remote-child ownership. A lost
connection does not permit the caller to replace or terminate an independently
owned remote peer. This evidence is not physical cross-machine/production
partition qualification, Windows support or a hard kernel resource boundary.

The adjacent query/merger recovery gate uses normal DrasiLib children and real
RocksDB, not an alternate child evaluator:

```sh
cargo test --locked --offline -p drasi-lib --no-default-features \
  --features computation-rocksdb-tests \
  --test computation_query_recovery --test computation_time_merge \
  -- --test-threads=1
```

Both Unix platforms passed the 51 selected cases. Their parent drivers externally
kill children at six query reset/deprovision and eight merger commit/handoff
boundaries, verify exclusive live storage ownership, and require exact recovery.
Ignored child workers count only through their passing parent drivers.
