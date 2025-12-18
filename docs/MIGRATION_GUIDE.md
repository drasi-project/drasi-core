# Migration Guide: Converting Middleware to Plugins

This guide helps existing middleware developers understand how the new plugin system affects their work and how to prepare for future plugin support.

## Current State (v0.2.0)

As of this release, the plugin system infrastructure is in place, but **no immediate changes are required** for existing middleware. All current middleware continues to work exactly as before:

- Built-in middleware (map, decoder, jq, etc.) remain unchanged
- Registration in `MiddlewareTypeRegistry` works as before
- No breaking changes to the `SourceMiddleware` or `SourceMiddlewareFactory` traits

## What's New

The plugin system adds:

1. **Plugin Discovery**: Ability to scan directories for middleware plugins
2. **Plugin Loaders**: Infrastructure for loading plugins at runtime
3. **Registry Extensions**: `MiddlewareTypeRegistry` can now load plugins dynamically

## Future: Creating Plugins (Planned)

When WASM plugin loading is implemented, you'll be able to convert existing middleware to plugins:

### Step 1: Create Plugin Project

```bash
cargo new --lib my_middleware_plugin
cd my_middleware_plugin
```

### Step 2: Configure for WASM

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
drasi-plugin-api = "0.1"  # Future API crate
```

### Step 3: Copy Middleware Implementation

Your existing `SourceMiddleware` implementation can be copied almost as-is:

```rust
// Before (built-in middleware):
use drasi_core::interface::{SourceMiddleware, SourceMiddlewareFactory};

pub struct MyMiddleware {
    config: MyConfig,
}

#[async_trait]
impl SourceMiddleware for MyMiddleware {
    async fn process(&self, change: SourceChange, index: &dyn ElementIndex) 
        -> Result<Vec<SourceChange>, MiddlewareError> {
        // Your logic here
    }
}

// After (plugin): 
// Nearly identical, just use drasi-plugin-api instead
use drasi_plugin_api::{SourceMiddleware, SourceMiddlewareFactory};

pub struct MyMiddleware {
    config: MyConfig,
}

#[async_trait]
impl SourceMiddleware for MyMiddleware {
    async fn process(&self, change: SourceChange, index: &dyn ElementIndex) 
        -> Result<Vec<SourceChange>, MiddlewareError> {
        // Same logic!
    }
}
```

### Step 4: Add Plugin Entry Point

```rust
// This is the only new code required
#[no_mangle]
pub extern "C" fn create_factory() -> Box<dyn SourceMiddlewareFactory> {
    Box::new(MyMiddlewareFactory::new())
}
```

### Step 5: Build and Deploy

```bash
cargo build --target wasm32-wasi --release
cp target/wasm32-wasi/release/my_middleware_plugin.wasm /path/to/plugins/
```

## Backward Compatibility

The plugin system is designed with backward compatibility in mind:

### For Middleware Developers

- **Existing Code**: No changes required
- **Trait Interfaces**: Remain stable
- **Build Process**: Unchanged for built-in middleware

### For Application Developers

- **Opt-in**: Plugin loading is optional
- **Mixed Mode**: Can use both built-in and plugin middleware simultaneously
- **Gradual Migration**: Convert to plugins at your own pace

## Deprecation Timeline

Built-in middleware will **NOT** be removed. The plugin system is additive:

- **Current (v0.2.0)**: Infrastructure in place
- **Next Release**: WASM loader implementation
- **Future**: Both built-in and plugin middleware supported indefinitely

## Advantages of Converting to Plugins

When plugin support is complete, converting to plugins offers:

1. **Independent Versioning**: Update middleware without updating drasi-core
2. **Faster Development**: No need to rebuild entire drasi-core
3. **Private Distribution**: Keep proprietary middleware separate
4. **Language Flexibility**: Future support for non-Rust plugins via WASM

## Current Limitations

As of v0.2.0, the following are **not yet implemented**:

- ❌ WASM runtime integration
- ❌ Plugin API crate (drasi-plugin-api)
- ❌ Dynamic library loading
- ❌ Plugin manifest format
- ❌ Hot reloading

## Questions?

For questions or feedback:

- Open an issue on [GitHub](https://github.com/drasi-project/drasi-core/issues)
- See [PLUGIN_SYSTEM.md](PLUGIN_SYSTEM.md) for detailed documentation
- Check [examples/plugin_system.rs](../examples/plugin_system.rs) for usage examples

## Summary

**TL;DR**: 
- No action required now
- Plugin system infrastructure is ready
- Actual plugin loading coming in future releases
- When ready, converting to plugins will be straightforward
- Built-in middleware remains fully supported
