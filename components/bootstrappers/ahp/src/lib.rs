#![allow(unexpected_cfgs)]
// Copyright 2026 The Drasi Authors.
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

//! Agent Host Protocol (AHP) bootstrap provider for Drasi.
//!
//! Connects to an AHP host, reads the root state, the session catalogue,
//! each selected session, and the automation catalogue, and emits the
//! resulting graph as inserts. It shares its graph mapping with
//! `drasi-source-ahp` through `drasi-ahp-common`, so element IDs and
//! properties match what the source streams afterwards.
//!
//! # Example
//!
//! ```no_run
//! use drasi_bootstrap_ahp::AhpBootstrapProvider;
//!
//! # fn main() -> anyhow::Result<()> {
//! let provider = AhpBootstrapProvider::builder()
//!     .with_url("ws://localhost:4321")
//!     .with_providers(vec!["copilot".to_string()])
//!     .build()?;
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod descriptor;
pub mod provider;

pub use config::AhpBootstrapConfig;
pub use provider::{AhpBootstrapProvider, AhpBootstrapProviderBuilder};

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "ahp-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::AhpBootstrapDescriptor],
);
