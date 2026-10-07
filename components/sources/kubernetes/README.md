# Kubernetes Source

Kubernetes source plugin for Drasi that watches Kubernetes resources and emits graph change events for continuous queries.

## Features

- Watches core/apps resources (`Pod`, `Deployment`, `ReplicaSet`, `Node`, `Service`, `ConfigMap`, `Namespace`)
- Emits `Insert`/`Update`/`Delete` based on Kubernetes watch events
- Optional owner-reference relationship emission (`OWNS`)
- Native nested property mapping with `Object` and `List` values
- Supports kubeconfig path, kubeconfig content, or in-cluster auth

## Configuration

```json
{
  "resources": [
    { "apiVersion": "v1", "kind": "ConfigMap" },
    { "apiVersion": "v1", "kind": "Pod" }
  ],
  "namespaces": ["default"],
  "authMode": "kubeconfig",
  "kubeconfigPath": "/home/user/.kube/config",
  "includeOwnerRelations": false,
  "startFrom": { "type": "now" }
}
```

### Important options

- `resources` (required): list of watched resource types
- `namespaces`: empty means all namespaces (cluster-wide watch for namespaced resources)
- `startFrom`: `now`, `beginning`, or `timestamp`
- `excludeAnnotations`: additional annotation keys to strip from emitted properties

Default excluded annotations:

- `kubectl.kubernetes.io/last-applied-configuration`
- `control-plane.alpha.kubernetes.io/leader`

## Lifecycle and access

`start()` loads the client configuration, checks both list and watch access for
every configured resource/namespace, and restores persisted UID state before
reporting `Running`. Initialization has a 10-second deadline and can be cancelled
by `stop()`; concurrent startup attempts are rejected. Invalid configuration,
unreadable or malformed persisted state, and fatal API responses (400,
401, 403, 404, or 422) fail startup with `Error`; transient failures are retried
within that deadline. The running watcher also reports fatal API errors as
`Error`, while continuing to retry transient failures and expired resource
versions.

Label and field selectors are trusted operator configuration. They are forwarded
to Kubernetes on both list and watch requests; Kubernetes validates their syntax.
Component error status messages contain a summary only; detailed failures are
returned to the caller and logged. Watch access probes classify failures by HTTP
status without reading upstream error bodies.

For Pod-only access in a single namespace, use `authMode: "incluster"`, explicitly
set `namespaces`, and grant the mounted ServiceAccount `list` and `watch` on
`pods` in that namespace. Omitting `namespaces` requests cluster-wide access.

`stop()` signals graceful shutdown and joins the source task, aborting and awaiting
its termination if it has not exited within five seconds. It clears stale
dispatcher state on every call, including after failures, and is safe to repeat.

## Query examples

```cypher
MATCH (p:Pod)
WHERE 'nginx:latest' IN p.containerImages
RETURN p.name, p.namespace
```

```cypher
MATCH (cm:ConfigMap)
WHERE cm.data.database_url STARTS WITH 'postgres://'
RETURN cm.name, cm.namespace
```

## Development

```bash
make build
make test
make integration-test
```

The integration test is ignored by default and requires Docker (k3s testcontainer).
