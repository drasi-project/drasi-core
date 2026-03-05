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

## Dynamic Plugins

Drasi Core includes an `xtask` build tool for building, listing, and publishing dynamic plugins — shared libraries (`.so`/`.dylib`/`.dll`) loaded at runtime by [Drasi Server](https://github.com/drasi-project/drasi-server).

### What Makes a Crate a Plugin?

A crate is automatically discovered as a dynamic plugin if it meets **both** criteria:

1. **Has the `dynamic-plugin` feature** defined in its `Cargo.toml`
2. **Follows the naming convention** `drasi-{type}-{kind}`, where `{type}` is one of `source`, `reaction`, or `bootstrap`

For example, `drasi-source-postgres`, `drasi-reaction-log`, `drasi-bootstrap-mssql`.

### Prerequisites

- Rust toolchain (see `rust-toolchain.toml`)
- System dependencies: `jq`, `libjq-dev`, `protobuf-compiler` (Linux) or `jq`, `protobuf` (macOS)
- For cross-compilation: [`cross`](https://github.com/cross-rs/cross) (Linux host with Docker)

### xtask Commands

#### `list-plugins` — Discover and list all dynamic plugins

Scans the workspace for crates matching the plugin criteria and prints each plugin's type, kind, version, and manifest path. Also shows the workspace SDK, Core, and Lib versions.

```bash
cargo run -p xtask -- list-plugins
# or
make list-plugins
```

#### `build-plugins` — Build plugin shared libraries

Builds all discovered plugin crates as `cdylib` shared libraries. Each plugin binary is placed under `target/<profile>/plugins/` (or `target/<triple>/<profile>/plugins/` for cross-builds), along with a `metadata.json` sidecar file containing plugin metadata.

**Flags:**

| Flag | Description |
|------|-------------|
| `--release` | Build in release mode (default: debug) |
| `--jobs N` / `-j N` | Number of parallel build jobs |
| `--target TRIPLE` | Cross-compile for a target triple (e.g. `aarch64-unknown-linux-gnu`) |

**Examples:**

```bash
# Build all plugins (debug)
make build-plugins

# Build all plugins (release)
make build-plugins-release

# Cross-compile for ARM Linux
cargo run -p xtask -- build-plugins --release --target aarch64-unknown-linux-gnu
```

**Cross-compilation behavior:**
- On Linux hosts, uses `cross` (Docker-based) for Linux and Windows targets.
- On macOS hosts, uses `cargo` directly. Only macOS targets are supported; Linux/Windows targets will exit with a clear error message.
- Cross-arch builds on the same OS (e.g. macOS x86 → macOS ARM) use `cargo` with `--target`.

**Generated metadata (`metadata.json`):**

A JSON file is written alongside each plugin binary with the following fields:

```json
{
  "name": "drasi-source-postgres",
  "kind": "postgres",
  "type": "source",
  "version": "0.1.8",
  "sdk_version": "0.1.0",
  "core_version": "0.1.0",
  "lib_version": "0.1.0",
  "target_triple": "x86_64-unknown-linux-gnu",
  "description": "...",
  "license": "Apache-2.0"
}
```

#### `publish-plugins` — Publish plugins as OCI artifacts

Publishes built plugins to an OCI container registry. Each plugin is pushed as an OCI artifact with two layers:

| Layer | Media Type |
|-------|-----------|
| Plugin binary (`.so`/`.dylib`/`.dll`) | `application/vnd.drasi.plugin.v1+binary` |
| Metadata JSON | `application/vnd.drasi.plugin.v1+metadata` |

After publishing all plugins, the command also updates the **plugin directory** — a special OCI package (`drasi-plugin-directory`) where each tag represents a known plugin (e.g. `source.postgres`, `reaction.storedproc-mssql`). This enables plugin discovery without knowing plugin names in advance.

**Flags:**

| Flag | Description |
|------|-------------|
| `--registry <URL>` | OCI registry (default: `ghcr.io/drasi-project`) |
| `--plugins-dir <DIR>` | Override the plugins directory |
| `--release` | Look in the release build directory |
| `--target <TRIPLE>` | Specify target triple for locating cross-compiled plugins |
| `--tag <TAG>` | Override the version tag for all plugins |
| `--pre-release <LABEL>` | Append a pre-release label (e.g. `dev.1`) |
| `--arch-suffix <SUFFIX>` | Append an architecture suffix to the tag (e.g. `linux-amd64`) |
| `--dry-run` | Show what would be published without pushing |

**Examples:**

```bash
# Dry run
make publish-plugins-dry-run ARCH_SUFFIX=linux-amd64

# Publish release build for a single architecture
make publish-plugins-release ARCH_SUFFIX=linux-amd64

# Publish with a pre-release label
make publish-plugins-release ARCH_SUFFIX=linux-amd64 PRE_RELEASE=dev.1

# Publish to a custom registry
make publish-plugins-release ARCH_SUFFIX=linux-amd64 REGISTRY=ghcr.io/my-org
```

### Authentication

Set the following environment variables:

```bash
export OCI_REGISTRY_USERNAME=<your-github-username>
export OCI_REGISTRY_PASSWORD=<your-pat-with-write-packages-scope>
```

The PAT needs the `write:packages` scope, and your GitHub account needs write access to the target org's packages.

### Tag Format

Plugins use **platform-suffixed tags** — the architecture is always appended as a tag suffix. There is no multi-arch manifest index; clients auto-append the correct suffix when pulling.

| Format | Example |
|--------|---------|
| Release | `ghcr.io/drasi-project/source/postgres:0.1.8-linux-amd64` |
| Pre-release | `ghcr.io/drasi-project/source/postgres:0.1.8-dev.1-linux-amd64` |
| Musl | `ghcr.io/drasi-project/source/postgres:0.1.8-linux-musl-amd64` |

Supported architecture suffixes:

| Suffix | Target Triple |
|--------|--------------|
| `linux-amd64` | `x86_64-unknown-linux-gnu` |
| `linux-arm64` | `aarch64-unknown-linux-gnu` |
| `linux-musl-amd64` | `x86_64-unknown-linux-musl` |
| `linux-musl-arm64` | `aarch64-unknown-linux-musl` |
| `windows-amd64` | `x86_64-pc-windows-gnu` |
| `darwin-amd64` | `x86_64-apple-darwin` |
| `darwin-arm64` | `aarch64-apple-darwin` |

### Plugin Directory

A special OCI package called `drasi-plugin-directory` is maintained in the registry. Each tag represents a known plugin using `{type}.{kind}` format (e.g. `source.postgres`, `reaction.storedproc-mssql`). The `.` separator is used because plugin types never contain dots, avoiding ambiguity with dashes in plugin kind names.

This enables the `plugin search` command in Drasi Server to discover available plugins by listing directory tags, then fetching version information from each matching plugin package.

### Publishing All Architectures

The `publish-all` target builds and publishes plugins for all 7 supported architectures in sequence:

```bash
# Publish all architectures
make publish-all

# Dry run
make publish-all-dry-run

# With pre-release label
make publish-all PRE_RELEASE=dev.1
```

### CI Workflow

The `.github/workflows/publish-plugins.yml` workflow automates publishing across all 7 architectures. It uses a build matrix:

- Linux targets (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) build on `ubuntu-latest` via `cross`
- macOS targets (`x86_64-apple-darwin`, `aarch64-apple-darwin`) build on `macos-latest` via `cargo`
- Windows target (`x86_64-pc-windows-gnu`) builds on `ubuntu-latest` via `cross`

After all builds succeed, a visibility step sets all packages (including `drasi-plugin-directory`) to public on GHCR. Trigger the workflow via `workflow_dispatch` in the GitHub Actions UI.

### Makefile Reference

| Target | Description |
|--------|-------------|
| `make list-plugins` | List all discovered plugin crates |
| `make build-plugins` | Build all plugins (debug) |
| `make build-plugins-release` | Build all plugins (release) |
| `make test-host-sdk` | Build test plugins and run host-sdk integration tests |
| `make publish-plugins` | Publish plugins (debug build) |
| `make publish-plugins-release` | Publish plugins (release build) |
| `make publish-plugins-dry-run` | Preview what would be published |
| `make publish-all` | Build and publish for all 7 architectures |
| `make publish-all-dry-run` | Dry run of `publish-all` |

All publish targets accept optional variables: `REGISTRY`, `PRE_RELEASE`, `ARCH_SUFFIX`.

### Running Host-SDK Integration Tests

```bash
# Build test plugins and run integration tests
make test-host-sdk
```

## Storage implementations

Drasi maintains internal indexes that are used to compute the effect of a data change on the query result. By default these indexes are in-memory, but a continuous query can be configured to use persistent storage.  Currently there are storage implementations for Redis, Garnet and RocksDB.


## Release Status

The drasi-core library is one component of an early release of Drasi which enables the community to learn about and experiment with the platform. Please let us know what you think and open Issues when you find bugs or want to request a new feature. Drasi is not yet ready for production workloads.


## Contributing

Please see the [Contribution guide](CONTRIBUTING.md)
