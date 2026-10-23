# Temporal reaction

`drasi-reaction-temporal` maps Drasi query diffs to explicit Temporal actions.
It calls Temporal through the official Rust client in-process. There is no
bridge, broadcast, subscriber registry, or automatic workflow creation.

Each `added`, `updated`, or `deleted` mapping selects one action. Any action
can be assigned to any operation. The workflow type and application handlers
belong to your Temporal Worker, not this reaction.

The [Docker-only inventory demo](../../../examples/lib/temporal-actions/README.md)
shows PostgreSQL changes becoming Temporal signals.

## Configuration

The reaction ID and subscribed query IDs belong to the host configuration.
This object contains only the reaction's settings:

```yaml
endpoint: http://localhost:7233
namespace: default
timeoutMs: 10000

outputTemplates:
  routes:
    monitored-results:
      added:
        action: signal
        workflowId: "{{after.workflowId}}"
        signalName: condition_entered
        template: '{{json after}}'
      updated:
        action: update
        workflowId: "{{after.workflowId}}"
        updateName: revise_condition
        waitFor: completed
        template: '{{json after}}'
      deleted:
        action: cancel
        workflowId: "{{before.workflowId}}"
        reason: "No longer matches {{query_id}}"
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `endpoint` | `http://localhost:7233` | Temporal gRPC endpoint as an HTTP or HTTPS URL. No path, credentials, query, or fragment. |
| `namespace` | `default` | Existing Temporal namespace. |
| `timeoutMs` | `10000` | Positive deadline for connection, run resolution, or an action, including SDK retries. |
| `priorityQueueCapacity` | `10000` | Positive bound on the reaction's in-memory queue. Durable query output provides recovery. |
| `outputTemplates.routes` | `{}` | Per-query operation mappings. |
| `outputTemplates.defaultTemplate` | Absent | Shared fallback operation mappings. |
| `apiKey` | Absent | API key. Requires HTTPS. |
| `tls` | Absent | Optional certificate settings. Requires HTTPS. HTTPS uses TLS even without this object. |

For each operation, lookup checks the exact query ID, its last dotted segment,
then `defaultTemplate`. Missing operations produce no action. Aggregation
diffs use `updated`; `Noop` produces no action. No action is inferred from the
operation name.

Unknown fields, invalid action combinations, invalid template syntax, and
routes unrelated to the subscribed queries fail construction.

## Actions

`workflowId` is required for every action. It is a Handlebars template, not
a prescribed query field or ID convention. Non-start actions accept an
optional `runId` template.

| `action` | Additional required fields | Optional fields | Acknowledgment |
| --- | --- | --- | --- |
| `signal` | `signalName` | `runId`, `template` | Server accepted the signal. The handler may not have run. |
| `start` | `workflowType`, `taskQueue` | `template`, `conflictPolicy`, `reusePolicy` | Server accepted the start, or selected an existing execution under `use_existing`. |
| `update` | `updateName` | `runId`, `template`, `waitFor` | The named Temporal Update reached the requested stage. |
| `cancel` | None | `runId`, `reason` | Server accepted cooperative cancellation. The workflow may still be running. |
| `terminate` | `reason` | `runId`, `template` | Server terminated the execution. No workflow cleanup runs. |

Signal, Update, Cancel, and Terminate never start a missing workflow.
Signal-With-Start and Update-With-Start are not supported.

### Start

```yaml
action: start
workflowId: "case-{{after.id}}"
workflowType: ReviewCase
taskQueue: reviews
conflictPolicy: fail
reusePolicy: reject_duplicate
template: '{{json after}}'
```

`conflictPolicy` defaults to `fail`. `use_existing` acknowledges an existing
running execution without sending the new input to it.

`reusePolicy` defaults to `reject_duplicate`. `allow_duplicate` permits
a new execution after the previous one closes. `allow_duplicate_failed_only`
permits reuse after failure, cancellation, termination, or timeout, but not
successful completion.

These policies apply within Temporal's retained workflow history. This reaction
does not provide a permanent workflow-ID registry.

### Update

Update calls a named Temporal Update handler. It does not edit arbitrary
workflow state. A responsive Worker is required for both acknowledgment modes.

`waitFor: completed` is the default. It waits for successful handler completion
and reports validator rejection or handler failure as a delivery error.
`waitFor: accepted` waits for acceptance but not the eventual outcome.
An already-returned failure is still an error.

Update return values are discarded. They are not inserted into Drasi.
A timeout does not retract an admitted Update; it may finish after the caller
has stopped waiting.

### Cancel and Terminate

Cancel accepts an optional reason and no nonempty payload template.
Cancellation depends on workflow cooperation and can run cleanup.

Terminate requires a nonempty reason. An optional `template` supplies JSON
termination details:

```yaml
action: terminate
workflowId: "{{before.workflowId}}"
reason: "Administrative removal"
template: '{{json before}}'
```

Termination does not run workflow cleanup. Already-running Activities may
continue even after the workflow execution has terminated.

Before either destructive action, the reaction persists the target's exact
run ID. Retries retain that run, even if a new execution has the same workflow
ID. An execution already in the requested terminal state counts as converged.
A missing execution or a different terminal state is an error.

## Templates and payloads

The template context contains `query_id`, `query_name`, `operation`,
`sequence_id`, `timestamp`, `metadata`, `before`, `after`, `data`, `row`, and
`request_id`. `operation` is `ADD`, `UPDATE`, or `DELETE`. `row` is `after`
for additions and updates, and `before` for deletions. Aggregation's `before`
can be null.

`{{json value}}` emits JSON without HTML escaping. Custom templates must
render valid JSON. Signal, Start, and Update send that value as one
`json/plain` argument. Custom payloads receive no extra wrapper.

An omitted or empty payload template uses the standard Drasi envelope:

```json
{
  "queryId": "monitored-results",
  "operation": "UPDATE",
  "sequenceId": 12,
  "timestamp": "2026-01-01T00:00:00+00:00",
  "before": {"workflowId": "monitor", "value": 1},
  "after": {"workflowId": "monitor", "value": 2}
}
```

ADD includes `after`; DELETE includes `before`. Nonempty metadata is included.
Cancel has no payload. Terminate with no template has no details.

Payload render errors or invalid JSON log a warning and fall back to the
standard envelope, following the shared reaction template contract.
Destination, action-name, and reason render errors instead stop delivery.
The reaction never substitutes a workflow ID or silently drops a mapped row.

`request_id` is generated for each mapped diff and persists across retries.
Including it in a custom payload lets application handlers record their own
deduplication key.

## Persistence, replay, and errors

This is a durable, future-only reaction. The host must provide persistent
query output and a durable state store. A new reaction starts at the current
query boundary rather than dispatching a snapshot of existing results.
Recovery uses `ReactionRecoveryPolicy::Strict`.

The reaction renders every mapped row before sending any action for an
emission. It persists the resulting commands, request IDs, and payloads before
contacting Temporal. Each successful command advances persisted progress;
the query checkpoint advances only after the entire emission succeeds.

One pending plan is retained per subscribed query. The next checkpointed
emission replaces it. Cancel and Terminate also persist their resolved run IDs
before sending their requests.

This closes the tested crash windows without claiming exactly-once delivery.
Temporal deduplication depends on the action, execution, and retained server
state. A timeout can leave acceptance unknown. Losing Drasi's state store also
loses its retry identities.

Signal and Update without `runId` address the current execution on each
attempt. A retry can therefore reach a newer execution with the same workflow
ID. Explicit `runId` confines those actions to one execution. Destructive
actions always retain their resolved run.

The loop processes actions serially in received emission order, preserving
order within each query. There is no cross-query causal ordering guarantee.
Slow Updates block later actions until completion or timeout. The first
delivery error stops the loop, logs the error, and sets the reaction status to
`Error` without advancing the failed emission's checkpoint.

Recovery requires correcting the cause and restarting the reaction or host.
Changing the endpoint, namespace, query configuration, or effective mappings
with unfinished delivery fails closed. Restoring the original configuration
allows replay. Authentication and deadline settings can change without
reinterpreting pending actions.

The persistent state partition must have one active reaction owner. There is
no distributed fencing or multi-writer coordination. State contains rendered
payloads and routing configuration. Host configuration persistence also
includes credentials or their references; both stores require access control.

## Authentication

The descriptor resolves existing Drasi `ConfigValue` references for the
endpoint, namespace, deadlines, queue capacity, API key, and TLS strings.
For example:

```yaml
endpoint:
  kind: EnvironmentVariable
  name: TEMPORAL_ENDPOINT
namespace:
  kind: EnvironmentVariable
  name: TEMPORAL_NAMESPACE
apiKey:
  kind: EnvironmentVariable
  name: TEMPORAL_API_KEY
```

The endpoint value must use `https://` when credentials are present.
Temporal Cloud requires the endpoint and namespace assigned to the account.
Self-hosted TLS settings depend on the server's authentication configuration.

`tls.serverRootCaCert` contains optional PEM root certificates;
`tls.domain` overrides the TLS server name.
`tls.clientIdentity.certificate` and `tls.clientIdentity.privateKey`
contain PEM contents, not file paths. Both are required for mutual TLS.
These fields also accept environment or secret references through the
descriptor.

The local end-to-end suite uses an unauthenticated development server.
It does not verify a Temporal Cloud account, API-key authentication, or mutual
TLS against a live endpoint.

## Rust and plugin interfaces

Embedded hosts construct `TemporalReaction::builder(id)` with subscribed
query IDs and a `TemporalReactionConfig`. `TemporalReactionDescriptor`
constructs the same reaction from host JSON and publishes the `temporal`
kind and namespaced configuration schemas.

The `dynamic-plugin` feature exports a cdylib. The end-to-end example uses the
embedded path; dynamic packaging is build-checked, not covered by that suite.
The existing shared SDK can leave a dynamically loaded reaction's background
error absent from the host's cached status. This component does not modify
that SDK. Dynamic hosts must not treat that cached status alone as proof of
successful delivery.
