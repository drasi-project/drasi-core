#![allow(unexpected_cfgs)]
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

//! Host-side SDK for loading and interacting with Drasi cdylib plugins.
//!
//! This crate provides:
//! - [`PluginLoader`] — scan directories and load cdylib plugins
//! - Proxy types that wrap FFI vtables into real DrasiLib traits
//!   ([`SourceProxy`], [`ReactionProxy`], [`SourcePluginProxy`], etc.)
//! - [`StateStoreVtableBuilder`] — build a `StateStoreVtable` from a host `StateStoreProvider`
//! - Callback types for log/lifecycle event capture

pub mod callbacks;
pub mod identity_bridge;
pub mod loader;
pub mod proxies;
#[cfg(feature = "registry")]
pub mod registry;
pub mod state_store_bridge;

pub use callbacks::{CallbackContext, CapturedLifecycle, CapturedLog, InstanceCallbackContext};
pub use identity_bridge::IdentityProviderVtableBuilder;
pub use loader::{LoadedPlugin, PluginLoader, PluginLoaderConfig};
pub use proxies::bootstrap_provider::{BootstrapPluginProxy, BootstrapProviderProxy};
pub use proxies::reaction::{ReactionPluginProxy, ReactionProxy};
pub use proxies::source::{SourcePluginProxy, SourceProxy};
pub use state_store_bridge::StateStoreVtableBuilder;
