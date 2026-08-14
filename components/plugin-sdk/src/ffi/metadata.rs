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
/// - `0.11.0`: snapshot rows cross the snapshot iterator as a
///   `KeyedSnapshotRow { k: <row_signature>, v: <row> }` JSON envelope instead
///   of a bare row, so the engine's canonical `row_signature` survives FFI
///   (fixes #605 duplicate dashboard rows on the plugin path).
/// - `0.12.0`: `FfiPluginRegistration` gained a trailing `set_log_level`
///   field. The host reports its effective log level and the plugin drops
///   more verbose records before formatting or crossing the FFI (#685).
/// - `0.13.0`: `BootstrapProviderVtable::bootstrap_fn` returns an
///   `FfiBootstrapStream` (push-based event and result receivers) immediately
///   instead of streaming through an `FfiBootstrapSender` in a blocking
///   run-to-completion call (fixes #686 unbounded buffering and pinned
///   async workers). `FfiBootstrapResult` also carries optional provider
///   error text (`error_ptr`/`error_len`/`error_drop_fn`) so failures
///   surface with their message instead of only a negative code.
/// - `0.14.0`: `StateStoreVtable` appends `compare_and_swap_fn` for atomic
///   state transitions needed by cross-replica fencing. The CAS result is a
///   POD status code (`FfiCompareAndSwapResult`) to avoid cross-dylib
///   ownership transfer for newly introduced operations.
/// - `0.15.0`: `StateStoreVtable` appends `is_durable_fn` so dynamic plugins
///   can distinguish persistent host stores from in-memory providers. The
///   nullable callback fails closed when the capability is unavailable.
pub const FFI_SDK_VERSION: &str = "0.15.0";

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
