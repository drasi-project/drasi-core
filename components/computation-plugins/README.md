# Native ComputationGraph components

Components are organized by role, with **one component or bootstrap provider per crate**. Build and load
only the components your solution uses; there is no standard or network bundle.
Each dynamic library owns its configuration, factory, implementation and export.
Shared crates contain helpers, not additional registered components.

The ten dynamic plugins use the lightweight `drasi-computation-contracts` and
`drasi-core-types` packages, not `drasi-lib` or the query engine. The SDK exposes
only supported plugin operations and borrowed host services. A query-result
message does not require query execution. `make check-native-dependencies`
checks every native library independently, including its build dependencies.

| Role | Crate | Component kind | Hosting |
|---|---|---|---|
| Bootstrap | [drasi-computation-bootstrap-sqlite](bootstrappers/sqlite) | `drasi.sqlite/file-bootstrap` | Dynamic or Rust SDK |
| Source | [drasi-computation-source-counter](sources/counter) | `drasi.standard/volatile-counter` | Dynamic or Rust SDK |
| Source | [drasi-computation-source-http](sources/http) | `drasi.network/http-source` | Dynamic or Rust SDK |
| Source | [drasi-computation-source-grpc](sources/grpc) | `drasi.network/grpc-source` | Dynamic or Rust SDK |
| Source | [drasi-computation-source-bridge](sources/bridge) | `drasi.bridge/source` | Static Rust factory |
| Transformer | [drasi-computation-transformer-middleware](transformers/middleware) | `drasi.standard/middleware` | Dynamic or Rust SDK |
| Transformer | [drasi-computation-transformer-arithmetic](transformers/arithmetic) | `drasi.standard/arithmetic` | Dynamic or Rust SDK |
| Sink | [drasi-computation-sink-capture](sinks/capture) | `drasi.standard/capture` | Dynamic or Rust SDK |
| Sink | [drasi-computation-sink-http](sinks/http) | `drasi.network/http-sink` | Dynamic or Rust SDK |
| Sink | [drasi-computation-sink-grpc](sinks/grpc) | `drasi.network/grpc-sink` | Dynamic or Rust SDK |
| Sink | [drasi-computation-sink-sse](sinks/sse) | `drasi.network/sse-sink` | Dynamic or Rust SDK |
| Sink | [drasi-computation-sink-bridge](sinks/bridge) | `drasi.bridge/sink` | Static Rust factory |

## Build just one plugin

From the `drasi-core` workspace:

```sh
cargo build -p drasi-computation-sink-sse --features dynamic-plugin
```

Load `target/debug/libdrasi_computation_sink_sse.dylib` on macOS,
`libdrasi_computation_sink_sse.so` on Linux, or `drasi_computation_sink_sse.dll` on
Windows. This library exposes **only** `drasi.network/sse-sink`; it does not
register HTTP/gRPC sources or sinks. Its dependency graph does not require their
component crates, gRPC, protobuf generation, `protoc`, `drasi-lib`, or the query
engine and query-language parsers.

Use the same command with another dynamic component's package name. Without
`dynamic-plugin`, its Rust `plugin()` definition is available without exporting
the C entrypoint. The two bridge endpoint crates retain their existing static
hosting contract: register the chosen crate's `factory()` in a normal
`FactoryRegistry`. They do not export a dynamic ABI.

For contributors, `make build-native-plugins` builds the ten separate libraries;
`make build-native-standard-plugins` and `make build-native-network-plugins` build
the corresponding test selections. `make build-native-sqlite-plugins` builds the
SQLite file bootstrap library. These are build conveniences, not bundles.
Bootstrap-only libraries expose factories through the bootstrap extension, not
a dummy entry in the processing-component list.

## Inspect configuration

Each dynamic component publishes descriptions, defaults and supported values from
its own configuration declaration. With the plugins loaded in Drasi Server,
`GET /api/v1/plugins/computation` returns their version-qualified
`configurationSchemas`. The kind-schema endpoint and OpenAPI use the same schema.
`detailsAvailable` distinguishes these descriptions from an older binary's
field/type-only metadata.

For example, gRPC batching is advertised as `batchSize: 1`, not as an unrestricted
integer, and adaptive ingress is restricted to `false`. These descriptions do not
enable missing features. Rebuild the component library to publish the new details;
loading an older native ABI 1.0 library remains supported.

## Shared code and contracts

- [common/network](common/network) shares source admission/lifecycle, configuration
  validation and wire projection. Protocol dependencies are opt-in; it exports no
  factories.
- [common/bridge](common/bridge) shares protocol framing, transport, TLS, controls
  and durable mapping state. Actual source/sink components and registration live
  in their own endpoint crates.
- [STANDARD.md](STANDARD.md) documents the counter/middleware/capture pipeline and
  arithmetic transaction contract.
- [NETWORK.md](NETWORK.md) documents the HTTP/gRPC wire, admission and completion
  contracts. The [SSE crate](sinks/sse) also has a standalone usage guide.

Implementation names, implementation version `"1"`, configuration version `1`,
ports, record schemas and processing semantics are unchanged. The native plugin
ABI remains **1.0.0**, independently of the workspace package version and the
legacy Source/Reaction ABI.

## Migrating from the bundled libraries

Replace each old `drasi-computation-standard` or `drasi-computation-network`
artifact with the particular component libraries your graph uses. Remove the
obsolete bundle from the plugin directory so discovery cannot register both its
factories and their replacements.

Rust bridge hosts replace `drasi_computation_bridge::factories()` with the
individual endpoint crates' `factory()` calls. Their re-exported configuration
and resource types come from the same shared bridge crate, preserving host
resource type identity.

Each new dynamic plugin's identity is its package name, for example
`drasi-computation-sink-sse`. Readable `kind`/version references stay the same.
Expanded topology and transaction recipes also contain plugin provenance, so
they must be re-exported using the new factories. The new libraries deliberately
do not impersonate the old bundle identities. Do not rewrite processing state,
discard retained obligations or weaken provenance checks to bypass an
incompatible persisted definition.
