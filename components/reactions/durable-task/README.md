# Durable Task Scheduler reaction

`drasi-reaction-durable-task` sends query-result changes directly to Durable Task
Scheduler over gRPC. The operator chooses an action for each query's ADD, UPDATE,
and DELETE. No bridge service is required.

The reaction supports DTS-backed workers, including classic .NET
`DurableTask.Core.TaskHubWorker` with `Microsoft.DurableTask.AzureManagedBackend`.
It is not a generic client for classic DTFx on Azure Storage, SQL Server, or Service
Fabric. It does not require Azure Durable Functions.

The [runnable example](../../../examples/lib/durable-task/README.md) uses the local
DTS emulator and a real .NET worker. The crate builds as a static library or a
dynamic plugin with `--features dynamic-plugin`. Its plugin kind is `durable-task`.

## Configuration

The descriptor accepts camelCase JSON. The same structure works as the plugin's
configuration in hosts that accept YAML:

```yaml
endpoint: "${DTS_ENDPOINT}"
taskHub: orders
producerId: orders-production-v1
authentication: identity
defaultTemplate:
  added:
    action: start
    orchestration: OrderWorkflow
    instanceId: "order-{{after.orderId}}-{{after.episode}}"
    onConflict: error
  updated:
    action: raiseEvent
    instanceId: "order-{{after.orderId}}-{{after.episode}}"
    eventName: OrderChanged
    onMissing: error
    onTerminal: error
  deleted:
    action: raiseEvent
    instanceId: "order-{{before.orderId}}-{{before.episode}}"
    eventName: OrderRemoved
    onMissing: ignore
    onTerminal: ignore
```

There is no built-in ADD-to-start or DELETE-to-terminate mapping. Each action is
valid in any operation slot. Omitted slots fall back through the routing rules;
an operation with no configured action is ignored.

| Field | Default | Contract |
|---|---|---|
| `endpoint` | Required | DTS origin URL, without a path, credentials, query, or fragment. |
| `taskHub` | Required | Task hub sent in gRPC metadata. |
| `producerId` | Required | Stable namespace for command IDs. A different query-sequence generation needs a new value. |
| `authentication` | `identity` | `identity` requires HTTPS and an injected identity provider. `none` permits only loopback HTTP for the emulator. |
| `scope` | `https://durabletask.io/.default` | Token audience requested from the identity provider. |
| `timeoutMs` | `10000` | Connection, credential acquisition, and individual RPC timeout. |
| `maxRetries` | `3` | Additional attempts after transient gRPC failures, at most 10. |
| `retryDelayMs` | `250` | Initial retry delay, 1 through 30000 ms. Exponential delay is capped at 30 seconds. |
| `priorityQueueCapacity` | `10000` | Maximum inbound queue size. Must be positive. |
| `recoveryPolicy` | `strict` | `strict` stops on delivery failures or outbox gaps. `autoSkipGap` explicitly permits lost deliveries. |
| `defaultTemplate` | None | `added`, `updated`, and `deleted` operation specifications. |
| `routes` | Empty | Per-query overrides with the same operation specifications. |

`endpoint`, `taskHub`, `producerId`, `scope`, `timeoutMs`, and
`priorityQueueCapacity` accept SDK `ConfigValue` literals, environment references,
and secret references. Secret references require the host's registered
`SecretResolver`. Descriptor-built reactions preserve those references in
`properties()`. Embedded construction persists the resolved configuration.

## Actions and target policies

Each operation specification has an `action` and an optional JSON `template`.

| Action | Fields | Behavior |
|---|---|---|
| `start` | `orchestration`, optional `version`, optional `instanceId`, `onConflict` | Schedule an instance. An omitted `instanceId` produces `drasi-<commandId>`, stable across retries. The default version is empty. |
| `raiseEvent` | `instanceId`, `eventName`, `onMissing`, `onTerminal` | Send an external event to an existing instance. |
| `terminate` | `instanceId`, optional `recursive`, `onMissing` | Request forced termination. `recursive` defaults to false. An already terminal instance satisfies the operation. |
| `ignore` | None | Acknowledge the result without contacting DTS. |

`onConflict`, `onMissing`, and `onTerminal` each accept `error` or `ignore`, with
`error` as the default. `ignore` logs the condition and acknowledges the operation.
`error` fails the emission; Strict recovery retains its previous checkpoint.

`start` never replaces an existing instance, even after completion. An identical
retry succeeds after comparing the existing orchestration name, version, input,
and `drasi-command-id` tag. A different command using that ID follows `onConflict`.
This distinguishes retry reconciliation from silently accepting an ID collision.
Replacement is not an automatic retry behavior: a fast-completing workflow could
otherwise execute twice after a lost start response. A new business episode can
use a new templated ID, or each change can use an automatically generated ID.

An instance ID is request-routing information, not a required field in source
records. Templates derive it from business keys when correlation is needed.
Rendered instance IDs, orchestration names, and event names must be nonempty
and contain no control characters.
Instance IDs must not use the durable-entity `@` prefix. DTS enforces any further
backend-specific limits.

For independent workflows on every change, all three slots can use `start`:

```yaml
defaultTemplate:
  added:
    action: start
    orchestration: HandleAddition
  updated:
    action: start
    orchestration: HandleUpdate
  deleted:
    action: start
    orchestration: HandleRemoval
```

For forced termination on removal:

```yaml
defaultTemplate:
  updated:
    action: ignore
  deleted:
    action: terminate
    instanceId: "order-{{before.orderId}}-{{before.episode}}"
    recursive: false
    onMissing: ignore
```

Termination does not undo completed activities or run application compensation.
A removal from a query result does not necessarily mean the source entity was
deleted. DELETE carries the previous matching row.

## Payloads and templates

The default input or event payload is one JSON item per diff:

```json
{
  "commandId": "26e6c8a0-b3e4-56dc-8c6a-aeae4d9117d4",
  "queryId": "orders",
  "sequenceId": 42,
  "timestamp": "2026-10-22T00:00:00Z",
  "operation": "UPDATE",
  "before": {"orderId": "17", "amount": 10},
  "after": {"orderId": "17", "amount": 20}
}
```

ADD includes `after`; UPDATE includes `before` and `after`; DELETE includes
`before`. Nonempty query metadata appears as `metadata`. Aggregation diffs use the
UPDATE slot and include `before` only when available. Noop and empty emissions
send nothing.

`template` changes the JSON body. `orchestration`, `version`, `instanceId`, and
`eventName` also support Handlebars. The template context contains `query_id`,
`query_name`, `sequence_id`, `command_id`, `operation`, `timestamp`, and `metadata`.
It includes `before`, `after`, and the raw UPDATE `data` when those values exist.
`{{json value}}` produces JSON-safe values. Preserve `command_id` in custom event
payloads if the workflow uses it for deduplication.

Resolution is per operation: full query ID, last dotted segment, default
specification, then ignore. An explicit `ignore` stops fallback. Unknown route
keys and invalid template syntax fail construction.

A body render error or invalid JSON logs a warning and uses the canonical
envelope for that row, following the reaction plugin contract. A routing-field
render error fails the emission rather than sending to an unrelated target.
The reaction renders the whole emission before sending its first operation.

## Authentication

Identity mode uses the existing Drasi `IdentityProvider`, injected through the
runtime or `Reaction::set_identity_provider`. Azure managed identity, workload
identity, and developer credentials can use `drasi-identity-azure`.
The reaction requests a token for `scope` before every RPC attempt; the provider
owns token caching and refresh. It sends the token as sensitive `authorization`
metadata, together with `taskhub` and `x-user-agent`.

HTTPS uses native trust roots. No certificate-verification bypass is provided.
The identity needs DTS permissions to start, inspect, signal, and terminate
instances in the selected hub. See Microsoft's
[DTS authentication documentation](https://learn.microsoft.com/en-us/azure/durable-task/scheduler/durable-task-scheduler-identity).
The local emulator path proves protocol interoperability, not Azure role
assignment, token refresh against Entra ID, or sovereign-cloud compatibility.

## Delivery and recovery boundaries

The reaction declares `is_durable() == true`. The host requires persistent query
storage and a persistent state store. Persistent queries also need replayable
sources. The .NET example writes to PostgreSQL and configures its native CDC
source, RocksDB indexes/outbox, and a redb state store.
Source replay uses PostgreSQL's retained WAL.

The shared `ReactionBase` loop processes emissions in order and checkpoints only
after all configured operations succeed or are explicitly ignored. Transient
gRPC failures retry with bounded exponential backoff. Exhausted retries,
permanent errors, and checkpoint failures stop delivery under Strict recovery.
Restart resumes from the persisted checkpoint.

Connections are lazy. Running status means the processing loop is ready;
connection failures are reported when delivery is attempted.

Command IDs are UUIDv5 values over `producerId`, reaction ID, full query ID,
sequence, diff position, row signature, and operation. They remain stable during
outbox replay. There is no separate inbox service. Operational constraints remain:

- A crash after DTS accepts a request but before checkpoint persistence can
  resend it. Starts reconcile using their ID and tag. Workflows must deduplicate
  external events by command ID.
- A successful `RaiseEvent` means enqueueing, not consumption. The reaction
  checks missing and terminal state before sending, but completion can race that
  check. DTS may return OK for an event that the completed instance never handles.
- If an event is accepted, its response is lost, and the workflow completes
  before retry, a terminal-error policy stops for reconciliation. The reaction
  cannot infer whether the workflow consumed that event.
- Query outbox retention is bounded and is not pinned by the slowest reaction.
  Strict recovery reports a retention gap rather than inventing missing changes.
- Purging DTS history removes start-deduplication evidence. Retain instances for
  at least the possible replay horizon. Reusing an instance ID can also route late
  events to a different execution.
- Keep producer IDs, reaction IDs, query IDs, and mappings stable during replay.
  Resetting query sequences needs a new producer epoch and an explicit recovery
  plan. Automatic reset is rejected; it cannot recover old command identity.
- Multiple producers need distinct producer IDs. Per-reaction processing order
  does not establish global order across writers.
- Fresh subscription does not bootstrap existing query rows. Configure the
  reaction before admitting source changes, or seed the required workflows.
  A PostgreSQL snapshot populates query state without emitting ADD deliveries;
  subsequent live changes use the configured actions.
- Workflow replay protects orchestration state, not arbitrary external effects.
  Activities still need idempotent or transactional effect handling.

Deprovisioning clears only local reaction state. It never terminates or purges
remote workflows.

## Protocol and verification

The unmodified protobuf and MIT license in `proto/` come from
[`microsoft/durabletask-protobuf` at `882583c1177706e148c3b146d4b2ba1c1d242e4c`](https://github.com/microsoft/durabletask-protobuf/blob/882583c1177706e148c3b146d4b2ba1c1d242e4c/protos/orchestrator_service.proto).
The protobuf SHA-256 is
`430b70583ffa54d9bab416b0254d146832ef87f947eefafd26a087d17b82904f`.
`build.rs` generates tonic bindings locally and needs `protoc`. Generated
wire types are private. Drasi maintains this client integration; it is not an
official Microsoft Rust SDK.

Targeted checks:

```sh
cargo test -p drasi-reaction-durable-task
cargo clippy -p drasi-reaction-durable-task --all-targets --features dynamic-plugin -- -D warnings
```

The tests include lost responses, duplicate starts, target policies, exact retry
payloads, bounded retry exhaustion, persistent checkpoints, partial-emission
replay, descriptor round-trips, and token acquisition.

### Live integration tests

```sh
dotnet run --project components/reactions/durable-task/tests/dotnet
```

This separate suite requires the [example's prerequisites](../../../examples/lib/durable-task/README.md#run-it).
It reuses the .NET local PostgreSQL/DTS setup but owns its `RecoveryWorkflow`,
command-ID bookkeeping, idempotent file sink, and worker crash harness.
The Rust fixture in `tests/native/` owns its recovery table and query, snapshot
bootstrap, configurable source slots, and checkpoint/outbox controls. None of those test controls is compiled
into the example. Append `-- --skip-rust-build` to reuse an already-built
recovery host; the final walkthrough check builds and uses the actual example
host.

Expect `PASS all 19 checks`. They cover early-event buffering, duplicate starts
and events, worker SIGKILL/replay, Drasi outbox recovery, PostgreSQL WAL replay and
snapshot before-images, physical row deletion, all four configurable actions,
and the actual guided example's prompts/cancellation/output. The example check
observes one workflow through placed, paid, shipped, and delivered, with delivery
arriving as an UPDATE rather than a removal. The recovery
scenarios must produce exactly sixteen file effects; the guided business workflow
reports to the console instead.

Diagnostics are retained under the example's ignored `artifacts/` directory.
Each run owns and removes its containers. The database and DTS remain running
while workers/Drasi restart; their restart persistence and managed Azure
authentication are not tested. Worker recovery can take several minutes, so the
suite allows four minutes.

Earlier POC runs also verified classic DTFx workers using the DTS backend; this
does not extend the reaction to other classic storage providers.
