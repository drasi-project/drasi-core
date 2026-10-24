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

mod activation_credentials;
mod admission;
mod bootstrap;
mod bootstrap_coordination;
mod consumer;
mod consumer_progress;
mod control;
mod factory;
mod input_position;
mod loader;
mod progress;
mod proxy;
mod query_snapshots;
mod source_schema;
mod transaction;

pub use activation_credentials::{NativeIdentityProviders, NativeIdentityResource};
pub use bootstrap::{NativeBootstrapFactories, NativeBootstrapFactory, NativeBootstrapProxy};
pub use bootstrap_coordination::{
    NativeSourceBootstrapFactories, NativeSourceBootstrapProgress, NativeSourceBootstrapResource,
};
pub use consumer::{NativeConsumerProxy, NativeConsumerResource};
/// Native computation ABI version this host loads.
pub use drasi_computation_plugin_abi::ABI_VERSION;
pub use drasi_computation_plugin_sdk::bootstrap::BootstrapFactoryMetadata;
pub use drasi_computation_plugin_sdk::configuration::{
    configuration_json_schema, ConfigDetails, ConfigFieldDetails,
};
pub use drasi_computation_plugin_sdk::metadata::{
    Capabilities, ConfigField, ConfigSchema, ConfigType, FactoryMetadata, PluginMetadata,
};
pub use drasi_computation_plugin_sdk::transport::Failure as NativeFailure;
pub(crate) use factory::runtime_configuration_schema;
pub use factory::NativeFactory;
pub use loader::{load, try_load, NativePlugin};
pub(crate) use loader::{try_load_library, try_read_metadata};
pub use proxy::NativeComponentProxy;
pub use query_snapshots::NativeQuerySnapshotsResource;

#[derive(Debug, Clone, serde::Serialize)]
pub struct NativeConsumerFactoryMetadata {
    pub implementation: drasi_lib::computation::v1::ImplementationIdentity,
    pub mode: drasi_computation_plugin_sdk::ConsumerMode,
}

#[cfg(test)]
mod activation_credentials_tests;
#[cfg(test)]
mod tests;
