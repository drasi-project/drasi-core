// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Independent native ComputationGraph plugin hosting. The legacy PluginLoader
//! and ABI 0.15 are unchanged. Try this family first during explicit discovery:
//! no native symbols returns None; any malformed native declaration is an error
//! and must not fall through to legacy registration.
//!
//! Loaded native code is trusted and process-pinned. There is no hot unloading,
//! graph execution manager, or task-local transfer between runtimes. The graph
//! owns scheduling and cancellation of every operation.

mod control;
mod factory;
mod loader;
mod proxy;
mod transaction;

pub use drasi_computation_plugin_sdk::metadata::{
    Capabilities, ConfigField, ConfigSchema, ConfigType, FactoryMetadata, PluginMetadata,
};
pub use drasi_computation_plugin_sdk::transport::Failure as NativeFailure;
pub use factory::NativeFactory;
pub use loader::{load, try_load, NativePlugin};
pub(crate) use loader::{try_load_library, try_read_metadata};
pub use proxy::NativeComponentProxy;

#[cfg(test)]
mod tests;
