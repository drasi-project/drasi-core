# Drasi Plugin System

## Overview

The Drasi middleware system now supports an extensible plugin architecture that allows developers to create and distribute middleware components independently of the core library. This enables:

- **Community Contributions**: Lower barrier to entry for adding new middleware
- **Independent Distribution**: Middleware can be packaged and versioned separately
- **Reduced Coupling**: Core library maintenance is simplified
- **Private Extensions**: Organizations can create proprietary middleware without modifying drasi-core

## Architecture

### Core Components

1. **PluginLoader Trait**: Defines the interface for discovering and loading plugins
2. **PluginDescriptor**: Metadata about a discovered plugin
3. **MiddlewareTypeRegistry**: Central registry that can load plugins dynamically
4. **FileSystemPluginLoader**: Default implementation that discovers plugins from a directory

### Plugin Formats

The system supports multiple plugin formats:

- **WebAssembly (WASM)**: Platform-independent, sandboxed execution (recommended for external plugins)
- **Dynamic Libraries**: Native performance for trusted plugins (platform-specific)

## Getting Started

### For Plugin Users

#### Loading Plugins at Runtime

```rust
use drasi_core::middleware::{
    MiddlewareTypeRegistry,
    fs_plugin_loader::FileSystemPluginLoader,
    plugin_loader::PluginLoaderConfig,
};
use std::path::PathBuf;

// Create a registry
let mut registry = MiddlewareTypeRegistry::new();

// Register built-in middleware factories
// registry.register(Arc::new(MyBuiltInFactory::new()));

// Configure plugin loader
let config = PluginLoaderConfig {
    plugin_dir: PathBuf::from("./plugins"),
    recursive: false,
    ..Default::default()
};

// Register the plugin loader
let loader = FileSystemPluginLoader::new(config);
registry.register_plugin_loader(Box::new(loader));

// Discover and load all plugins
match registry.discover_and_load_plugins() {
    Ok(count) => println!("Loaded {} plugins", count),
    Err(e) => eprintln!("Failed to load plugins: {}", e),
}

// Use middleware as usual
let factory = registry.get("my_plugin_middleware");
```

#### Plugin Directory Structure

```
./plugins/
├── my_middleware.wasm
├── custom_filter.wasm
└── data_enricher.wasm
```

### For Plugin Developers

#### Current Status

⚠️ **Note**: The plugin loading infrastructure is now in place, but format-specific loaders (WASM, dynamic library) are not yet implemented. The following sections describe the planned plugin development workflow.

#### Creating a WASM Plugin (Planned)

1. **Create a new Rust library project**:

```bash
cargo new --lib my_middleware_plugin
cd my_middleware_plugin
```

2. **Configure Cargo.toml**:

```toml
[package]
name = "my_middleware_plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
drasi-plugin-api = "0.1"  # Future: Plugin API definitions
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

3. **Implement the middleware**:

```rust
use drasi_plugin_api::*;  // Future API
use async_trait::async_trait;

pub struct MyMiddleware {
    config: MyMiddlewareConfig,
}

#[async_trait]
impl SourceMiddleware for MyMiddleware {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        // Your middleware logic here
        Ok(vec![source_change])
    }
}

// Plugin entry point
#[no_mangle]
pub extern "C" fn create_middleware_factory() -> Box<dyn SourceMiddlewareFactory> {
    Box::new(MyMiddlewareFactory::new())
}
```

4. **Build the plugin**:

```bash
cargo build --target wasm32-wasi --release
```

5. **Deploy the plugin**:

```bash
cp target/wasm32-wasi/release/my_middleware_plugin.wasm /path/to/drasi/plugins/
```

## Plugin Discovery

The `FileSystemPluginLoader` discovers plugins by:

1. Scanning the configured plugin directory
2. Identifying files by extension (`.wasm`, `.so`, `.dll`, `.dylib`)
3. Creating plugin descriptors with metadata
4. Attempting to load each discovered plugin

### Configuration Options

```rust
pub struct PluginLoaderConfig {
    /// Directory to scan for plugins
    pub plugin_dir: PathBuf,

    /// Whether to load plugins recursively from subdirectories
    pub recursive: bool,

    /// List of plugin formats to support
    pub supported_formats: Vec<PluginFormat>,

    /// Whether to fail if any plugin fails to load
    pub fail_on_error: bool,
}
```

## Custom Plugin Loaders

You can implement custom plugin loaders for different discovery mechanisms:

```rust
use drasi_core::middleware::plugin_loader::{PluginLoader, PluginDescriptor, PluginLoadError};
use drasi_core::interface::SourceMiddlewareFactory;
use std::sync::Arc;

pub struct MyCustomPluginLoader {
    // Your custom configuration
}

impl PluginLoader for MyCustomPluginLoader {
    fn discover_plugins(&self) -> Result<Vec<PluginDescriptor>, PluginLoadError> {
        // Implement your discovery logic
        // E.g., query a database, fetch from HTTP, read from config file
    }

    fn load_plugin(
        &mut self,
        descriptor: &PluginDescriptor,
    ) -> Result<Arc<dyn SourceMiddlewareFactory>, PluginLoadError> {
        // Implement your loading logic
    }

    fn loader_name(&self) -> &str {
        "my_custom_loader"
    }
}
```

## Security Considerations

### WASM Plugins (Recommended for External Code)

- **Sandboxed Execution**: WASM plugins run in isolated environments
- **No Direct System Access**: Plugins cannot access files, network, or system calls without explicit permission
- **Resource Limits**: Memory and CPU usage can be constrained
- **Portable**: Same binary works across all platforms

### Dynamic Library Plugins (Use Only for Trusted Code)

- **Full Process Access**: Native plugins have same privileges as host process
- **Platform-Specific**: Must compile separately for each OS/architecture
- **Higher Risk**: Plugin bugs can crash the host or corrupt memory
- **Best For**: First-party or highly trusted middleware

## Future Enhancements

The following features are planned for future releases:

1. **WASM Runtime Integration**: Complete implementation of WASM plugin loading using wasmtime
2. **Plugin API Crate**: Dedicated `drasi-plugin-api` crate with stable interfaces
3. **Plugin Manifest Format**: JSON/TOML manifests with version, dependencies, and metadata
4. **Plugin Signing**: Cryptographic verification of plugin authenticity
5. **Hot Reloading**: Update plugins without restarting the host process
6. **Plugin Registry**: Centralized repository for discovering and sharing plugins
7. **Development Tools**: CLI tools for scaffolding, building, and testing plugins
8. **Dynamic Library Support**: Implementation of native plugin loading with abi_stable

## Examples

### Example 1: Simple Transform Middleware Plugin

```rust
// Planned example - not yet functional
pub struct UpperCaseMiddleware {}

#[async_trait]
impl SourceMiddleware for UpperCaseMiddleware {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match source_change {
            SourceChange::Insert { mut element } => {
                // Transform all string properties to uppercase
                transform_properties(&mut element);
                Ok(vec![SourceChange::Insert { element }])
            }
            _ => Ok(vec![source_change]),
        }
    }
}
```

### Example 2: Custom Plugin Loader from Config File

```rust
// Planned example - not yet functional
pub struct ConfigFilePluginLoader {
    config_path: PathBuf,
}

impl PluginLoader for ConfigFilePluginLoader {
    fn discover_plugins(&self) -> Result<Vec<PluginDescriptor>, PluginLoadError> {
        let config: PluginConfig = read_config(&self.config_path)?;
        
        let descriptors = config.plugins
            .iter()
            .map(|p| PluginDescriptor {
                name: p.name.clone(),
                version: p.version.clone(),
                path: PathBuf::from(&p.path),
                format: p.format.clone(),
                metadata: p.metadata.clone(),
            })
            .collect();
        
        Ok(descriptors)
    }
    
    // ... rest of implementation
}
```

## Troubleshooting

### Plugin Not Discovered

- Verify the plugin file is in the configured plugin directory
- Check that the file extension matches a supported format (`.wasm`, `.so`, `.dll`, `.dylib`)
- Enable recursive scanning if plugins are in subdirectories
- Check logs for discovery errors

### Plugin Load Failed

- Ensure the plugin is compiled for the correct target (e.g., `wasm32-wasi`)
- Verify API version compatibility
- Check that all required dependencies are available
- Review plugin initialization logic for errors

### Performance Issues

- Profile middleware execution to identify bottlenecks
- Consider using native plugins for performance-critical operations
- Implement caching strategies in your middleware
- Optimize data serialization across plugin boundaries

## API Stability

The plugin system follows semantic versioning:

- **Major version changes**: May break compatibility, require plugin recompilation
- **Minor version changes**: Additive only, backward compatible
- **Patch version changes**: Bug fixes, fully compatible

Plugin authors should specify the minimum required API version in their plugin metadata.

## Contributing

Contributions to the plugin system are welcome! Areas of particular interest:

1. WASM runtime integration
2. Additional plugin loaders (HTTP, database, etc.)
3. Plugin development tools
4. Documentation improvements
5. Example plugins

See [CONTRIBUTING.md](../CONTRIBUTING.md) for guidelines.

## License

The Drasi plugin system is licensed under the Apache License 2.0. See [LICENSE](../LICENSE) for details.
