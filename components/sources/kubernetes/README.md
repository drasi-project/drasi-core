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

`start()` loads the client configuration and checks both list and watch access for
every configured resource/namespace before reporting `Running`. Initialization
has a 10-second deadline. Invalid configuration and fatal API responses (400,
401, 403, 404, or 422) fail startup with `Error`; transient failures are retried
within that deadline. The running watcher also reports fatal API errors as
`Error`, while continuing to retry transient failures and expired resource
versions.

For Pod-only access in a single namespace, use `authMode: "incluster"`, explicitly
set `namespaces`, and grant the mounted ServiceAccount `list` and `watch` on
`pods` in that namespace. Omitting `namespaces` requests cluster-wide access.

`stop()` cancels and joins the source task, including its abort fallback, before
returning. It also cleans up failed sources and is safe to repeat.

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
