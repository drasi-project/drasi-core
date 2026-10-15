// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional, read-only configuration descriptions. The base plugin metadata and
//! operation tables remain unchanged. Returned bytes live as long as the library.

use crate::{BorrowedBytes, Header};

pub const VERSION: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_configuration_v1\0";

#[repr(C)]
pub struct ConfigurationMetadataV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    /// UTF-8 JSON configuration descriptions, bounded by `MAX_METADATA_BYTES`.
    pub manifest: BorrowedBytes,
}

pub type ConfigurationFn = unsafe extern "C" fn() -> *const ConfigurationMetadataV1;
