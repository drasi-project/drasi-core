// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Plugin registration and discovery.
//!
//! A [`PluginRegistration`] is the entry point for a plugin shared library. When the
//! server loads a `.so`/`.dll`/`.dylib` file, it calls the `drasi_plugin_init()` export
//! which returns a `PluginRegistration` containing the plugin's descriptors.
//!
//! # For Statically-Linked Plugins
//!
//! Statically-linked plugins (built into the server binary) create their registration
//! programmatically:
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::prelude::*;
//!
//! pub fn register() -> PluginRegistration {
//!     PluginRegistration::new()
//!         .with_source(Box::new(MySourceDescriptor))
//!         .with_bootstrapper(Box::new(MyBootstrapDescriptor))
//! }
//! ```
//!
//! # For Dynamically-Loaded Plugins
//!
//! Dynamic plugins are compiled as shared libraries (`dylib`) and export a C-ABI
//! entry point. The function must return a heap-allocated `PluginRegistration` as a
//! raw pointer. The server takes ownership via `Box::from_raw`.
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::prelude::*;
//!
//! #[no_mangle]
//! pub extern "C" fn drasi_plugin_init() -> *mut PluginRegistration {
//!     let registration = PluginRegistration::new()
//!         .with_source(Box::new(MySourceDescriptor));
//!     Box::into_raw(Box::new(registration))
//! }
//! ```
//!
//! The `#[no_mangle]` attribute and `extern "C"` ABI are required so the server
//! can locate the symbol by name (`drasi_plugin_init`) in the shared library.
//!
//! # SDK Version Compatibility
//!
//! Each registration includes the SDK version it was built against. The server
//! checks this at load time and rejects plugins built with incompatible SDK versions.

use crate::descriptor::{
    BootstrapPluginDescriptor, ReactionPluginDescriptor, SourcePluginDescriptor,
};

/// The version of the Drasi Plugin SDK.
///
/// This is automatically embedded in every [`PluginRegistration`] and checked
/// by the server at load time for compatibility. If the server's `SDK_VERSION`
/// does not match the plugin's `SDK_VERSION`, the plugin is rejected during
/// dynamic loading.
///
/// For dynamic plugins, this means both the server and the plugin must be
/// compiled against the exact same version of the `drasi-plugin-sdk` crate.
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Contains the descriptors provided by a plugin.
///
/// A plugin can register any combination of source, reaction, and bootstrap descriptors.
/// For example, a "postgres" plugin might register both a `SourcePluginDescriptor` and
/// a `BootstrapPluginDescriptor`.
///
/// # Builder Pattern
///
/// ```rust,ignore
/// let registration = PluginRegistration::new()
///     .with_source(Box::new(PostgresSourceDescriptor))
///     .with_bootstrapper(Box::new(PostgresBootstrapDescriptor));
/// ```
///
/// # Role in Dynamic Loading
///
/// For dynamically-loaded plugins, a `PluginRegistration` is the value returned by
/// the `drasi_plugin_init()` entry point. The server:
///
/// 1. Loads the shared library (`.so`/`.dylib`/`.dll`).
/// 2. Resolves the `drasi_plugin_init` symbol.
/// 3. Calls it and takes ownership of the returned `PluginRegistration` via
///    [`Box::from_raw`].
/// 4. Validates that [`sdk_version`](Self::sdk_version) matches the server's
///    [`SDK_VERSION`]. Mismatches cause the plugin to be rejected.
/// 5. Extracts all descriptors and registers them in the plugin registry.
///
/// The entry point must return a `*mut PluginRegistration` created with
/// `Box::into_raw(Box::new(registration))`.
pub struct PluginRegistration {
    /// The SDK version this plugin was compiled against.
    pub sdk_version: &'static str,

    /// The Rust compiler version used to build this plugin.
    pub rust_version: &'static str,

    /// The Tokio version this plugin was compiled against.
    pub tokio_version: &'static str,

    /// Build compatibility hash from the shared runtime.
    /// Computed from rustc version, crate version, target triple, and profile.
    pub build_hash: &'static str,

    /// Source plugin descriptors provided by this plugin.
    pub sources: Vec<Box<dyn SourcePluginDescriptor>>,

    /// Reaction plugin descriptors provided by this plugin.
    pub reactions: Vec<Box<dyn ReactionPluginDescriptor>>,

    /// Bootstrap plugin descriptors provided by this plugin.
    pub bootstrappers: Vec<Box<dyn BootstrapPluginDescriptor>>,
}

impl PluginRegistration {
    /// Create a new empty registration with SDK and Rust version metadata.
    pub fn new() -> Self {
        Self {
            sdk_version: SDK_VERSION,
            rust_version: env!("DRASI_RUSTC_VERSION"),
            tokio_version: drasi_plugin_runtime::TOKIO_VERSION,
            build_hash: drasi_plugin_runtime::BUILD_HASH,
            sources: Vec::new(),
            reactions: Vec::new(),
            bootstrappers: Vec::new(),
        }
    }

    /// Register a source plugin descriptor.
    pub fn with_source(mut self, descriptor: Box<dyn SourcePluginDescriptor>) -> Self {
        self.sources.push(descriptor);
        self
    }

    /// Register a reaction plugin descriptor.
    pub fn with_reaction(mut self, descriptor: Box<dyn ReactionPluginDescriptor>) -> Self {
        self.reactions.push(descriptor);
        self
    }

    /// Register a bootstrap plugin descriptor.
    pub fn with_bootstrapper(mut self, descriptor: Box<dyn BootstrapPluginDescriptor>) -> Self {
        self.bootstrappers.push(descriptor);
        self
    }

    /// Returns true if this registration contains no descriptors.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty() && self.reactions.is_empty() && self.bootstrappers.is_empty()
    }

    /// Returns the total number of descriptors in this registration.
    pub fn descriptor_count(&self) -> usize {
        self.sources.len() + self.reactions.len() + self.bootstrappers.len()
    }
}

impl Default for PluginRegistration {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PluginRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistration")
            .field("sdk_version", &self.sdk_version)
            .field("rust_version", &self.rust_version)
            .field("tokio_version", &self.tokio_version)
            .field("build_hash", &self.build_hash)
            .field(
                "sources",
                &self.sources.iter().map(|s| s.kind()).collect::<Vec<_>>(),
            )
            .field(
                "reactions",
                &self.reactions.iter().map(|r| r.kind()).collect::<Vec<_>>(),
            )
            .field(
                "bootstrappers",
                &self
                    .bootstrappers
                    .iter()
                    .map(|b| b.kind())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::SourcePluginDescriptor;
    use async_trait::async_trait;

    struct DummySource;

    #[async_trait]
    impl SourcePluginDescriptor for DummySource {
        fn kind(&self) -> &str {
            "dummy"
        }
        fn config_version(&self) -> &str {
            "1.0.0"
        }
        fn config_schema_json(&self) -> String {
            "{}".to_string()
        }
        fn config_schema_name(&self) -> &str {
            "DummySourceConfig"
        }
        async fn create_source(
            &self,
            _id: &str,
            _config_json: &serde_json::Value,
            _auto_start: bool,
        ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
            anyhow::bail!("not implemented")
        }
    }

    #[test]
    fn test_registration_new_is_empty() {
        let reg = PluginRegistration::new();
        assert!(reg.is_empty());
        assert_eq!(reg.descriptor_count(), 0);
    }

    #[test]
    fn test_registration_with_source() {
        let reg = PluginRegistration::new().with_source(Box::new(DummySource));
        assert!(!reg.is_empty());
        assert_eq!(reg.descriptor_count(), 1);
        assert_eq!(reg.sources[0].kind(), "dummy");
    }

    #[test]
    fn test_registration_sdk_version() {
        let reg = PluginRegistration::new();
        assert_eq!(reg.sdk_version, SDK_VERSION);
    }

    #[test]
    fn test_registration_debug() {
        let reg = PluginRegistration::new().with_source(Box::new(DummySource));
        let debug = format!("{reg:?}");
        assert!(debug.contains("dummy"));
        assert!(debug.contains("sdk_version"));
    }
}
