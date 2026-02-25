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

//! Shared Tokio runtime bridge for Drasi dynamic plugin loading.
//!
//! This crate exists solely to provide a single shared copy of Tokio that both
//! the Drasi Server binary and dynamically-loaded plugin shared libraries link
//! against. Without this, each `cdylib` plugin would statically link its own
//! copy of Tokio, with its own thread-local runtime context — causing
//! `tokio::spawn()` and other Tokio APIs to fail inside plugin code.
//!
//! # How It Works
//!
//! This crate is built with `crate-type = ["lib", "dylib"]`:
//!
//! - **Static builds** (`cargo build`): Cargo picks the `lib` (rlib) version.
//!   Everything links statically as usual. This crate adds zero overhead.
//!
//! - **Dynamic builds** (`cargo xtask build-dynamic` with `-C prefer-dynamic`):
//!   Cargo picks the `dylib` version, producing `libdrasi_tokio_bridge.so`.
//!   Both the server binary and plugin `.so` files link against this shared
//!   library, sharing a single Tokio instance and its thread-local runtime
//!   context.
//!
//! # Why a Separate Crate?
//!
//! Putting Tokio in a crate with generic-heavy dependencies (like serde) causes
//! monomorphization issues: the dylib only contains monomorphizations for types
//! used within that crate, not types used by the server. By isolating Tokio in
//! a minimal crate with no generics, we avoid this entirely.
//!
//! # Version Pinning
//!
//! The Tokio version is pinned to an exact version (`=1.x.y`) to guarantee ABI
//! compatibility between the server and all plugins. Both must use the same
//! Rust toolchain and the same Tokio version.

/// The pinned Tokio version this bridge was built with.
///
/// Used by the plugin loader to validate that plugins were built with a
/// compatible Tokio version.
pub const TOKIO_VERSION: &str = "1.44.1";

// Re-export tokio so dependents use the exact same copy
pub use tokio;
