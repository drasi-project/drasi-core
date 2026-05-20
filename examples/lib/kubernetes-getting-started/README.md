# Kubernetes Getting Started (drasi-lib)

This example runs Drasi with the Kubernetes source and Kubernetes bootstrap provider, then logs query diffs for a demo `ConfigMap`.

## What it does

- Watches `ConfigMap` objects in namespace `default`
- Bootstraps existing matching objects at startup
- Runs query:

```cypher
MATCH (c:ConfigMap)
WHERE c.name = 'drasi-demo'
RETURN c.name AS name, c.namespace AS namespace, c.data.color AS color
```

- Prints `ADD`, `UPDATE`, and `DELETE` events to stdout via `LogReaction`

## Prerequisites

- Docker
- kubectl
- Rust toolchain

## Quick start

```bash
./quickstart.sh
```

In a second terminal, run:

```bash
./test-updates.sh
```

You should see add/update/delete output in the running example terminal.

## Scripts

- `setup.sh` — ensures a reachable Kubernetes cluster (starts local k3s if needed)
- `diagnose.sh` — cluster and resource diagnostics
- `test-updates.sh` — create/update/delete demo ConfigMap
- `quickstart.sh` — setup then run the example

## Notes

- By default, scripts use `~/.kube/config`
- If no cluster is reachable, `setup.sh` starts `rancher/k3s:v1.32.2-k3s1` as `drasi-k3s-example`
