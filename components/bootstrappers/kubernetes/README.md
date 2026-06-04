# Kubernetes Bootstrap Provider

Bootstrap provider for Kubernetes sources that lists currently matching resources and sends them as bootstrap events.

## Overview

This provider is designed to pair with `drasi-source-kubernetes` and reuses the source configuration to determine:

- resource kinds and apiVersions
- namespace scope
- label/field selectors
- authentication mode

For each listed Kubernetes object, it emits the same mapped node/relation shape used by the streaming source path.

## Plugin kind

`kubernetes`

## Configuration

Bootstrap configuration is intentionally empty:

```json
{}
```

The provider reads all operational settings from the parent Kubernetes source configuration.

## Usage from code

```rust
use drasi_bootstrap_kubernetes::KubernetesBootstrapProvider;

let bootstrap = KubernetesBootstrapProvider::builder()
    .with_source_config(source_config.clone())
    .build()?;
```

## Development

```bash
make build
make test
```
