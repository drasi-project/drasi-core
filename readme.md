# Drasi Core
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/10588/badge)](https://www.bestpractices.dev/projects/10588)

Drasi-core is the library used by [Drasi](https://github.com/drasi-project/drasi-platform) to implement continuous queries.

Continuous Queries, as the name implies, are queries that run continuously. To understand what is unique about them, it is useful to contrast them with the kind of instantaneous queries developers are accustomed to running against databases.

When you execute an instantaneous query, you are running the query against the database at a point in time. The database calculates the results to the query and returns them. While you work with those results, you are working with a static snapshot of the data and are unaware of any changes that may have happened to the data after you ran the query. If you run the same instantaneous query periodically, the query results might be different each time due to changes made to the data by other processes. But to understand what has changed, you would need to compare the most recent result with the previous result.

Continuous Queries, once started, continue to run until they are stopped. While running, Continuous Queries will process any changes flowing from one or more data sources, compute how the query result is affected and emit the diff.

Continuous Queries are implemented as graph queries written in the Cypher Query Language. The use of a declarative graph query language means you can express rich query logic that takes into consideration both the properties of the data you are querying and the relationships between data in a single query.

Drasi-core is the internal library used by [Drasi](https://github.com/drasi-project/drasi-platform) to implement continuous queries. Drasi itself is a much broader solution with many more moving parts.  Drasi-core can be used stand-alone from Drasi for embedded scenarios, where continuous queries could run in-process inside an application.

## Example

In this scenario, we have a set of `Vehicles` and a set of `Zones` where vehicles can be.  The conceptual data model in Drasi is a labeled property graph, so we will add the vehicles and zones as nodes in the graph and we will connect them with a `LOCATED_IN` relationship.

We will create a Continuous Query to observe the `Parking Lot` Zone so that we will get notified when any Vehicle enters or exits the Zone.

```cypher
MATCH 
    (v:Vehicle)-[:LOCATED_IN]->(:Zone {type:'Parking Lot'}) 
RETURN 
    v.color AS color, 
    v.plate AS plate
```

When the `LOCATED_IN` relationship is added or deleted, the Continuous Query will emit a diff stating that the Vehicle was added to or removed from the query result. And changing one of the Vehicle properties, such as the `color`, will cause the query to emit a diff stating the Vehicle has been updated.

Let's look at how to configure a Continuous Query using the `QueryBuilder`.

```rust
let query_str = "
    MATCH 
        (v:Vehicle)-[:LOCATED_IN]->(:Zone {type:'Parking Lot'}) 
    RETURN 
        v.color AS color, 
        v.plate AS plate";

let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
let parser = Arc::new(CypherParser::new(function_registry.clone()));
let query_builder = QueryBuilder::new(query_str, parser)
    .with_function_registry(function_registry);
let query = query_builder.build().await;
```

Let's load a Vehicle (v1) and a Zone (z1) as nodes into the query.
We can do this by processing a `SourceChange::Insert` into the query, this in turn takes an `Element`, which can be of either an `Element::Node` or `Element::Relation`, which represent nodes and relations in the graph model that can be queried.  When constructing an `Element`, you will also need to supply `ElementMetadata` which contains its unique identity (`ElementReference`), any labels to be applied to it on the labeled property graph and an effective from time.

```rust
query.process_source_change(SourceChange::Insert {
    element: Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("", "v1"),
            labels: Arc::new([Arc::from("Vehicle")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(json!({
            "plate": "AAA-1234",
            "color": "Blue"
        }))
    },
}).await;

query.process_source_change(SourceChange::Insert {
    element: Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("", "z1"),
            labels: Arc::new([Arc::from("Zone")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(json!({
            "type": "Parking Lot"
        })),
    },
}).await;
```

We can use the `process_source_change` function on the continuous query to compute the diff a data change has on the query result.


```rust
query.process_source_change(SourceChange::Insert {
    element: Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new("", "v1-location"),
            labels: Arc::new([Arc::from("LOCATED_IN")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::new(),
        out_node: ElementReference::new("", "z1"),
        in_node: ElementReference::new("", "v1"),
    },
}).await;
```

```
Result: [Adding { 
    after: {"color": String("Blue"), "plate": String("AAA-1234")} 
}]
```

```rust

query.process_source_change(SourceChange::Update {
    element: Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("", "v1"),
            labels: Arc::new([Arc::from("Vehicle")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(json!({
            "plate": "AAA-1234",
            "color": "Green"
        }))
    },
}).await;
```

```
Result: [Updating { 
    before: {"color": String("Blue"), "plate": String("AAA-1234")}, 
    after: {"color": String("Green"), "plate": String("AAA-1234")} 
}]
```

```rust
query.process_source_change(SourceChange::Delete {
    metadata: ElementMetadata {
        reference: ElementReference::new("", "v1-location"),
        labels: Arc::new([Arc::from("LOCATED_AT")]),
        effective_from: 0,
    },
}).await;
```

```
Result: [Removing { 
    before: {"color": String("Green"), "plate": String("AAA-1234")} 
}]
```

### Additional examples

More examples can be found under the [examples](examples) folder.

## Building Dynamic Plugins

Drasi Core includes an `xtask` build tool for building and publishing dynamic plugins (shared libraries loaded at runtime by [Drasi Server](https://github.com/drasi-project/drasi-server)).

### Prerequisites

- Rust toolchain (see `rust-toolchain.toml`)
- System dependencies: `jq`, `libjq-dev`, `protobuf-compiler` (Linux) or `jq`, `protobuf` (macOS)

### Build Commands

```bash
# List all discovered plugin crates
make list-plugins

# Build all plugins (debug)
make build-plugins

# Build all plugins (release)
make build-plugins-release

# Build for a specific target (cross-compilation)
cargo run -p xtask -- build-plugins --release --target aarch64-unknown-linux-gnu
```

Built plugins are placed under `target/<profile>/` (or `target/<triple>/<profile>/` for cross-builds).

### Running Host-SDK Integration Tests

```bash
# Build test plugins and run integration tests
make test-host-sdk
```

## Publishing Plugins to an OCI Registry

Plugins are published as OCI artifacts to a container registry (default: `ghcr.io/drasi-project`). Each architecture is published with a per-arch tag suffix, and a final merge step creates a multi-arch manifest index.

### Authentication

Set the following environment variables:

```bash
export OCI_REGISTRY_USERNAME=<your-github-username>
export OCI_REGISTRY_PASSWORD=<your-pat-with-write-packages-scope>
```

The PAT needs the `write:packages` scope, and your GitHub account needs write access to the target org's packages.

### Publish Workflow

**1. Build and publish per-architecture:**

```bash
# Dry run (no push)
make publish-plugins-dry-run ARCH_SUFFIX=linux-amd64

# Publish release build for a single architecture
make publish-plugins-release ARCH_SUFFIX=linux-amd64

# Publish with a pre-release label
make publish-plugins-release ARCH_SUFFIX=linux-amd64 PRE_RELEASE=dev.1

# Publish to a custom registry
make publish-plugins-release ARCH_SUFFIX=linux-amd64 REGISTRY=ghcr.io/my-org
```

**2. Merge per-arch tags into a multi-arch manifest index:**

After publishing all architectures, create the manifest index:

```bash
# Merge all default architectures (linux-amd64, linux-arm64, windows-amd64, darwin-amd64, darwin-arm64)
make merge-manifests

# Merge with pre-release label
make merge-manifests PRE_RELEASE=dev.1

# Dry run
make merge-manifests-dry-run

# Merge specific architectures only
make merge-manifests MERGE_ARCHS="linux-amd64 linux-arm64"
```

### Per-Architecture Tag Format

Each plugin is tagged with its own version from `Cargo.toml`:

- Per-arch: `ghcr.io/drasi-project/source/postgres:0.1.8-linux-amd64`
- Merged: `ghcr.io/drasi-project/source/postgres:0.1.8`
- Pre-release: `ghcr.io/drasi-project/source/postgres:0.1.8-dev.1`

### CI Workflow

The `.github/workflows/publish-plugins.yml` workflow automates the full publish pipeline across 5 architectures (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`) with a final manifest merge step. Trigger it via `workflow_dispatch` in the GitHub Actions UI.

## Storage implementations

Drasi maintains internal indexes that are used to compute the effect of a data change on the query result. By default these indexes are in-memory, but a continuous query can be configured to use persistent storage.  Currently there are storage implementations for Redis, Garnet and RocksDB.


## Release Status

The drasi-core library is one component of an early release of Drasi which enables the community to learn about and experiment with the platform. Please let us know what you think and open Issues when you find bugs or want to request a new feature. Drasi is not yet ready for production workloads.


## Contributing

Please see the [Contribution guide](CONTRIBUTING.md)
