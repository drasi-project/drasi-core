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

//! Drasi Plugin Runtime — shared dynamic library for Drasi plugins.
//!
//! This crate is compiled as a Rust `dylib` and serves as the single shared
//! library that both the Drasi server binary and all dynamic plugins link
//! against. By placing all common dependencies (tokio, serde, drasi-lib, etc.)
//! into one shared library, we ensure:
//!
//! 1. **Consistent symbol hashes** — generic monomorphizations (e.g.,
//!    `drop_in_place<String>`) are generated once in this dylib, and all
//!    consumers reference them with the same hash.
//!
//! 2. **Shared Tokio runtime** — the server and plugins share the same Tokio
//!    thread-local context, so `tokio::spawn()` inside plugin code finds the
//!    host's runtime.
//!
//! 3. **Reduced memory footprint** — common code exists once in memory instead
//!    of being duplicated in every plugin.
//!
//! # Build Requirements
//!
//! Both the server and plugins **must** be built with `-C prefer-dynamic` so
//! that the linker uses this dylib rather than statically linking separate
//! copies of the shared dependencies.
//!
//! # Pinned Tokio Version
//!
//! Tokio is pinned to an exact version (`=1.44.1`) because the server and
//! plugins must share the same Tokio runtime thread-locals. Different Tokio
//! versions may use incompatible internal representations.

/// Pinned Tokio version used by this runtime.
///
/// Plugins report this version in their [`PluginRegistration`] so the server
/// can verify compatibility at load time.
pub const TOKIO_VERSION: &str = "1.44.1";

/// Build compatibility hash — computed at compile time by `build.rs`.
///
/// Both the server and plugins embed this hash. The server checks it before
/// calling `plugin_init()` to catch mismatched builds early (preventing UB).
///
/// Components: rustc version, crate version, target triple, profile.
pub const BUILD_HASH: &str = env!("DRASI_BUILD_HASH");

// Re-export all shared dependencies so they are included in the dylib
// and available to consumers.
pub use tokio;
pub use serde;
pub use serde_json;
pub use anyhow;
pub use thiserror;
pub use async_trait;
pub use utoipa;
pub use drasi_lib;
pub use log;
pub use chrono;
pub use uuid;
pub use futures;
pub use ordered_float;
pub use tracing;
pub use tracing_subscriber;
pub use tracing_log;

/// Initialize the global tracing subscriber with an env filter and fmt layer.
///
/// Also installs the `tracing-log` bridge so `log::info!()` etc. are captured
/// by the tracing subscriber. Call this once in the host before loading plugins.
///
/// `default_level` is used when `RUST_LOG` is not set (e.g., `"info"`, `"debug"`).
pub fn init_tracing(default_level: &str) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_level(true)
                .with_thread_ids(true),
        )
        .init();

    // Bridge log crate → tracing (so log::info!() also goes through the subscriber)
    let _ = tracing_log::LogTracer::init();
}
