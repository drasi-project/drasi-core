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

#![allow(unexpected_cfgs)]
#![deny(unused)]

//! `drasi-ffi-primitives` — Core FFI-safe types and vtable generation macros.
//!
//! This crate provides the fundamental `#[repr(C)]` types and declarative macros
//! needed to build vtable-based FFI boundaries for Drasi's cdylib plugin system.
//!
//! **No domain-specific dependencies** — this crate depends only on `std`.
//!
//! # Types
//!
//! - [`FfiStr`] / [`FfiOwnedStr`] / [`FfiStringArray`] — String types for FFI
//! - [`FfiResult`] — Result type for FFI operations
//! - [`FfiGetResult`] — Result type for key-value store get operations
//! - [`AsyncExecutorFn`] — Type-erased async executor for cdylib runtimes
//! - [`SendPtr`] / [`SendMutPtr`] — Send-safe raw pointer wrappers
//!
//! # Macros
//!
//! - [`ffi_vtable!`] — Generate `#[repr(C)]` vtable structs from a method list
//! - [`vtable_fn_getter!`] — Generate synchronous getter `extern "C"` functions
//! - [`vtable_fn_async!`] — Generate async method `extern "C"` functions with thread+block_on
//! - [`impl_vtable_proxy!`] — Generate proxy structs that wrap vtables into method calls

pub mod macros;
pub mod types;

pub use types::*;

/// Marker embedded in every dynamic plugin that links the Rustls `ring`
/// provider. `xtask build-plugins` checks for this exact NUL-terminated value
/// in each staged cdylib before publication.
pub const RUSTLS_RING_PROVIDER_MARKER: [u8; 27] = *b"drasi-rustls-provider=ring\0";

/// Prefix of the NUL-terminated ordinary Source/Reaction ABI version that
/// `export_plugin!` embeds in every plugin binary.
///
/// Build, publication and registry tools read it without loading the binary or
/// running its initialization code, including for cross-compiled targets. The
/// native ComputationGraph family embeds its own marker
/// (`drasi_computation_plugin_abi::ABI_VERSION_MARKER_PREFIX`).
pub const PLUGIN_ABI_VERSION_MARKER_PREFIX: &[u8] = b"drasi-plugin-ffi-abi=";

/// Read the unique ordinary Source/Reaction ABI version embedded in a binary.
// Inline (here and below) so plugin rlibs carry no externally linked copy:
// MinGW auto-exports one and retains its search string, which would be
// mistaken for a second ABI marker.
#[inline]
pub fn read_plugin_abi_version(binary: &[u8]) -> Result<&str, &'static str> {
    read_abi_version_marker(binary, PLUGIN_ABI_VERSION_MARKER_PREFIX)
}

/// Read the unique NUL-terminated, non-empty UTF-8 version that follows
/// `prefix` in `binary`. A missing, unterminated, empty, non-UTF-8 or repeated
/// marker is an error: a binary's ABI is never guessed.
#[inline]
pub fn read_abi_version_marker<'a>(
    binary: &'a [u8],
    prefix: &[u8],
) -> Result<&'a str, &'static str> {
    let mut version = None;
    for (offset, window) in binary.windows(prefix.len()).enumerate() {
        if window != prefix {
            continue;
        }
        if version.is_some() {
            return Err("multiple plugin ABI version markers");
        }
        let value = &binary[offset + prefix.len()..];
        let end = value
            .iter()
            .position(|byte| *byte == 0)
            .ok_or("unterminated plugin ABI version marker")?;
        let value =
            std::str::from_utf8(&value[..end]).map_err(|_| "plugin ABI version is not UTF-8")?;
        if value.is_empty() {
            return Err("empty plugin ABI version marker");
        }
        version = Some(value);
    }
    version.ok_or("missing plugin ABI version marker")
}

#[cfg(test)]
mod abi_marker_tests {
    use super::*;

    #[test]
    fn reads_abi_version_from_binary_bytes() {
        assert_eq!(
            read_plugin_abi_version(b"\x7fELF\0drasi-plugin-ffi-abi=0.19.0\0suffix"),
            Ok("0.19.0")
        );
        assert_eq!(
            read_abi_version_marker(b"\0native-abi=0.1.0\0", b"native-abi="),
            Ok("0.1.0")
        );
    }

    #[test]
    fn rejects_missing_malformed_and_ambiguous_markers() {
        for binary in [
            b"legacy binary".as_slice(),
            b"drasi-plugin-ffi-abi=0.19.0",
            b"drasi-plugin-ffi-abi=\0",
            b"drasi-plugin-ffi-abi=\xff\0",
            b"drasi-plugin-ffi-abi=0.18.0\0drasi-plugin-ffi-abi=0.19.0\0",
            b"drasi-plugin-ffi-abi=0.19.0\0drasi-plugin-ffi-abi=0.19.0\0",
        ] {
            assert!(read_plugin_abi_version(binary).is_err(), "{binary:?}");
        }
    }
}
