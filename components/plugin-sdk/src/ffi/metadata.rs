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

//! Plugin metadata for version validation at load time.

use super::types::FfiStr;

/// The FFI ABI contract version of this SDK. Used for compatibility checks at
/// plugin load time: the host (`loader.rs`) rejects any plugin whose reported
/// `sdk_version` differs in the **major or minor** component from this value.
///
/// This is intentionally **decoupled from the crate release version**
/// (`CARGO_PKG_VERSION`): it identifies the layout/ABI of the `#[repr(C)]`
/// envelope structs and the wire format of serialized payloads, not the package
/// release. **Bump the minor (pre-1.0) on any `#[repr(C)]` layout change or
/// payload wire-format change.**
///
/// History:
/// - `0.10.0`: source change / bootstrap events now cross the boundary as
///   serialized (MessagePack) payloads instead of reinterpreted `repr(Rust)`
///   opaque pointers (fixes #602 cross-cdylib heap corruption).
pub const FFI_SDK_VERSION: &str = "0.10.0";

/// The target triple this crate was compiled for.
pub const TARGET_TRIPLE: &str = env!("TARGET_TRIPLE");

/// Git commit SHA the plugin was built from (short hash, e.g. "a1b2c3d").
pub const GIT_COMMIT_SHA: &str = env!("GIT_COMMIT_SHA");

/// Build timestamp in RFC 3339 format (e.g. "2026-03-03T17:00:00Z").
pub const BUILD_TIMESTAMP: &str = env!("BUILD_TIMESTAMP");

/// Metadata returned by `drasi_plugin_metadata()` for version validation.
/// The host checks these fields before calling `drasi_plugin_init()`.
#[repr(C)]
pub struct PluginMetadata {
    /// Version of the drasi-plugin-sdk crate (FFI envelope types).
    pub sdk_version: FfiStr,
    /// Version of drasi-core (opaque pointer types: SourceChange, Element, etc.).
    pub core_version: FfiStr,
    /// Version of drasi-lib (Source/Reaction traits, QueryResult, etc.).
    pub lib_version: FfiStr,
    /// Plugin's own version.
    pub plugin_version: FfiStr,
    /// Target triple (e.g., "x86_64-unknown-linux-gnu").
    pub target_triple: FfiStr,
    /// Git commit SHA the plugin was built from (short hash).
    pub git_commit: FfiStr,
    /// Build timestamp in RFC 3339 format.
    pub build_timestamp: FfiStr,
}
