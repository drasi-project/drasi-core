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
