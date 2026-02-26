# Drasi Plugin SDK

The Drasi Plugin SDK provides traits, types, and utilities for building plugins for the any application that implements the [Host SDK](../host-sdk/). Plugins extend the server with new data sources, reactions, and bootstrap providers.

Plugins can be compiled directly into the server binary (**static linking**) or built as shared libraries for **dynamic loading** at runtime.

## Quick Start

Add the SDK to your plugin crate:

```toml
[dependencies]
drasi-plugin-sdk = { workspace = true }
drasi-lib = { workspace = true }
```

Import the prelude:

```rust
use drasi_plugin_sdk::prelude::*;
```

## Plugin Types

The SDK defines three plugin categories, each with a corresponding descriptor trait:

| Plugin Type | Trait | Purpose |
|---|---|---|
| **Source** | `SourcePluginDescriptor` | Ingests data from external systems (databases, APIs, queues) |
| **Reaction** | `ReactionPluginDescriptor` | Consumes query results and performs side effects (webhooks, logging, stored procedures) |
| **Bootstrap** | `BootstrapPluginDescriptor` | Provides initial data snapshots so queries start with a complete view |

## Writing a Plugin

### 1. Define your configuration DTO

Use `ConfigValue<T>` for fields that may be provided as static values, environment variable references, or secret references. Derive `utoipa::ToSchema` for OpenAPI schema generation.

```rust
use drasi_plugin_sdk::prelude::*;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MySourceConfigDto {
    /// The hostname to connect to
    #[schema(value_type = ConfigValueString)]
    pub host: ConfigValue<String>,

    /// The port number
    #[schema(value_type = ConfigValueU16)]
    pub port: ConfigValue<u16>,

    /// Optional connection timeout in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU32>)]
    pub timeout_ms: Option<ConfigValue<u32>>,
}
```

### 2. Implement a descriptor trait

Each descriptor provides:

- **`kind()`** — A unique string identifier (e.g., `"postgres"`, `"http"`)
- **`config_version()`** — A semver version for the configuration DTO
- **`config_schema_json()`** — A JSON-serialized OpenAPI schema map
- **`config_schema_name()`** — The schema name used as the key in the OpenAPI spec
- **`create_*()`** — A factory method that builds a plugin instance from raw JSON config

```rust
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

pub struct MySourceDescriptor;

// Collect all transitive schemas via utoipa's OpenApi derive
#[derive(OpenApi)]
#[openapi(schemas(MySourceConfigDto))]
struct MySourceSchemas;

#[async_trait]
impl SourcePluginDescriptor for MySourceDescriptor {
    fn kind(&self) -> &str { "my-source" }
    fn config_version(&self) -> &str { "1.0.0" }

    fn config_schema_json(&self) -> String {
        let api = MySourceSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    fn config_schema_name(&self) -> &str { "MySourceConfigDto" }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: MySourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();
        let host = mapper.resolve_string(&dto.host)?;
        let port = mapper.resolve_typed(&dto.port)?;
        // ... build and return the source
        todo!()
    }
}
```

### 3. Register the plugin

#### For static plugins

Bundle your descriptors into a `PluginRegistration` and pass them to the server at startup:

```rust
use drasi_plugin_sdk::prelude::*;

pub fn register() -> PluginRegistration {
    PluginRegistration::new()
        .with_source(Box::new(MySourceDescriptor))
        .with_bootstrapper(Box::new(MyBootstrapDescriptor))
}
```

#### For dynamic plugins

Use the `export_plugin!` macro, which generates the FFI entry points, a dedicated tokio runtime, and log/lifecycle callback wiring:

```rust
drasi_plugin_sdk::export_plugin!(
    plugin_id = "my-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [MySourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [MyBootstrapDescriptor],
);
```

## Static vs. Dynamic Plugins

### Static Linking

Compile the plugin crate directly into the server binary. Call your `register()` function at server startup and pass the descriptors to the plugin registry. This is the simplest approach — no shared library boundary, no ABI concerns.

### Dynamic Loading

Build the plugin as a `cdylib` shared library that the server discovers and loads at runtime. This allows deploying new plugins without recompiling the server.

#### Step 1: Configure crate type

```toml
[lib]
crate-type = ["lib", "cdylib"]

[dependencies]
drasi-plugin-sdk = { workspace = true }
drasi-lib = { workspace = true }
```

The dual `["lib", "cdylib"]` crate type allows the crate to be used both as a normal Rust dependency (for static builds) and as a shared library (for dynamic loading).

#### Step 2: Use the `export_plugin!` macro

The `export_plugin!` macro generates everything needed for dynamic loading:

- `drasi_plugin_metadata()` — Returns version info for validation **before** initialization
- `drasi_plugin_init()` — Returns an `FfiPluginRegistration` with FFI vtable factories
- A dedicated tokio runtime for the plugin
- Log and lifecycle callback bridges (tracing integration via `FfiTracingLayer`)

```rust
drasi_plugin_sdk::export_plugin!(
    plugin_id = "postgres",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [PostgresSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [PostgresBootstrapDescriptor],
);
```

Each descriptor is wrapped in an FFI vtable (`SourcePluginVtable`, `ReactionPluginVtable`, `BootstrapPluginVtable`) that provides `#[repr(C)]` function pointers. When the host calls a factory method (e.g., `create_source_fn`), the plugin constructs a trait object and wraps it in a component vtable (`SourceVtable`, `ReactionVtable`, `BootstrapProviderVtable`) with its own FFI-safe function pointers.

#### Step 3: Build and deploy

```bash
# Build with dynamic plugin feature enabled
cargo build --release --features dynamic-plugin

# Copy the shared library to the server's binary directory
cp target/release/libdrasi_source_my_plugin.so /path/to/server/
```

#### Compatibility requirements

Both the plugin and the server **must** be compiled with:

- The **same Rust toolchain version** — the Rust ABI is not stable across compiler versions.
- The **same `drasi-plugin-sdk` version** — the server compares `SDK_VERSION` at load time and rejects mismatches.
- The **same target triple** — validated via `PluginMetadata::target_triple`.

The server performs a two-phase load:

1. **Metadata validation** — calls `drasi_plugin_metadata()` to check SDK version, build hash, and target triple.
2. **Initialization** — calls `drasi_plugin_init()` only if metadata validation passes.

## FFI Architecture

Dynamic plugins communicate with the host through `#[repr(C)]` vtable structs (stable C ABI). The FFI layer is organized into several modules:

| Module | Description |
|---|---|
| `ffi::types` | Core FFI-safe primitives (`FfiStr`, `FfiResult`, `FfiOwnedStr`, etc.). Re-exported from the `drasi-ffi-primitives` crate. |
| `ffi::vtables` | `#[repr(C)]` vtable structs for all component types, generated with the `ffi_vtable!` macro |
| `ffi::vtable_gen` | Functions that wrap trait objects into vtables (e.g., `build_source_vtable`) |
| `ffi::callbacks` | Log and lifecycle callback types (`LogCallbackFn`, `LifecycleCallbackFn`) |
| `ffi::metadata` | `PluginMetadata` struct and version constants for load-time validation |
| `ffi::tracing_bridge` | Bridges tracing/log events from the plugin to the host via FFI callbacks |
| `ffi::identity` | FFI types for `IdentityProvider` injection into plugins |
| `ffi::identity_proxy` | Plugin-side proxy that implements `IdentityProvider` over an FFI vtable |
| `ffi::state_store_proxy` | Plugin-side proxy that implements `StateStoreProvider` over an FFI vtable |
| `ffi::bootstrap_proxy` | Plugin-side proxy that implements `BootstrapProvider` over an FFI vtable |

Each plugin gets its own tokio runtime (created by the `export_plugin!` macro). Async operations are dispatched via the `AsyncExecutorFn` callback, which ensures futures run on the plugin's runtime rather than the host's.

## Configuration Values

`ConfigValue<T>` is the building block for plugin configuration fields. It supports three input formats:

### Static value

```yaml
host: "localhost"
port: 5432
```

### POSIX environment variable reference

```yaml
host: "${DB_HOST}"
port: "${DB_PORT:-5432}"
```

### Structured reference

```yaml
password:
  kind: Secret
  name: db-password
port:
  kind: EnvironmentVariable
  name: DB_PORT
  default: "5432"
```

### Type aliases

| Alias | Type |
|---|---|
| `ConfigValueString` | `ConfigValue<String>` |
| `ConfigValueU16` | `ConfigValue<u16>` |
| `ConfigValueU32` | `ConfigValue<u32>` |
| `ConfigValueU64` | `ConfigValue<u64>` |
| `ConfigValueUsize` | `ConfigValue<usize>` |
| `ConfigValueBool` | `ConfigValue<bool>` |

Each alias has a corresponding `*Schema` wrapper type for use with `#[schema(value_type = ...)]` annotations in utoipa.

## Resolving Configuration Values

Use `DtoMapper` to resolve `ConfigValue` references to their actual values:

```rust
let mapper = DtoMapper::new();

// Resolve to string
let host: String = mapper.resolve_string(&dto.host)?;

// Resolve to typed value (parses string → T via FromStr)
let port: u16 = mapper.resolve_typed(&dto.port)?;

// Resolve optional fields
let timeout: Option<u32> = mapper.resolve_optional(&dto.timeout_ms)?;

// Resolve a vec of string values
let tags: Vec<String> = mapper.resolve_string_vec(&dto.tags)?;
```

### Built-in resolvers

| Resolver | Handles | Behavior |
|---|---|---|
| `EnvironmentVariableResolver` | `ConfigValue::EnvironmentVariable` | Reads `std::env::var()`, falls back to default |
| `SecretResolver` | `ConfigValue::Secret` | Delegates to a pluggable `SecretResolver` registered via `register_secret_resolver()` |

### Custom resolvers

Implement `ValueResolver` to add custom resolution logic (e.g., HashiCorp Vault, AWS SSM):

```rust
use drasi_plugin_sdk::prelude::*;

struct VaultResolver { /* vault client */ }

impl ValueResolver for VaultResolver {
    fn resolve_to_string(
        &self,
        value: &ConfigValue<String>,
    ) -> Result<String, ResolverError> {
        match value {
            ConfigValue::Secret { name } => {
                // Look up secret in Vault
                Ok("resolved-secret-value".to_string())
            }
            _ => Err(ResolverError::WrongResolverType),
        }
    }
}

let mapper = DtoMapper::new()
    .with_resolver("Secret", Box::new(VaultResolver { /* ... */ }));
```

## DTO Versioning

Each plugin independently versions its configuration DTO via the `config_version()` method using semver:

| Change | Version Bump | Example |
|---|---|---|
| Field removed or renamed | **Major** | `1.0.0` → `2.0.0` |
| Field type changed | **Major** | `1.0.0` → `2.0.0` |
| New optional field added | **Minor** | `1.0.0` → `1.1.0` |
| Documentation or description change | **Patch** | `1.0.0` → `1.0.1` |

## OpenAPI Schema Generation

Plugins provide their configuration schemas as JSON-serialized utoipa schema maps. The server collects these from all registered plugins and assembles them into the OpenAPI specification. This keeps schema ownership with the plugins while allowing the server to build a unified API spec.

The recommended pattern is to use `#[derive(OpenApi)]` to automatically collect all transitive schema dependencies:

```rust
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(schemas(MySourceConfigDto))]
struct MyPluginSchemas;

fn config_schema_json(&self) -> String {
    let api = MyPluginSchemas::openapi();
    serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
}

fn config_schema_name(&self) -> &str {
    "MySourceConfigDto"
}
```

## Modules

| Module | Description |
|---|---|
| `config_value` | `ConfigValue<T>` enum, type aliases, and OpenAPI schema wrappers |
| `descriptor` | Plugin descriptor traits (`SourcePluginDescriptor`, `ReactionPluginDescriptor`, `BootstrapPluginDescriptor`) |
| `ffi` | FFI layer for dynamic plugin loading — vtables, callbacks, proxies, tracing bridge |
| `mapper` | `DtoMapper` service and `ConfigMapper` trait for DTO-to-domain conversions |
| `registration` | `PluginRegistration` struct, `SDK_VERSION`, `BUILD_HASH`, and `TOKIO_VERSION` constants |
| `resolver` | `ValueResolver` trait and built-in implementations |
| `prelude` | Convenience re-exports for plugin authors |

## License

Licensed under the [Apache License, Version 2.0](http://www.apache.org/licenses/LICENSE-2.0).
