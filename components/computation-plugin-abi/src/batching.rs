// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional host-owned sink collection. Base metadata and table layouts are
//! unchanged. Call policy only at a serialized component-operation boundary.

use crate::{Header, Reply};
use std::ffi::c_void;

pub const VERSION: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_batching_v1\0";
pub type BatchingFn = unsafe extern "C" fn() -> *const PluginBatchingV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginBatchingV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    /// Bounded MessagePack bool for a borrowed plugin handle's factory index.
    pub factory: Option<unsafe extern "C" fn(*mut c_void, u32) -> Reply>,
    /// Bounded MessagePack Option<SinkBatching> for a borrowed component handle.
    pub policy: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn extension_preserves_frozen_base_tables() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<PluginBatchingV1>(), 24 + 2 * pointer);
        assert_eq!(size_of::<crate::ComponentVTable>(), 16 + 5 * pointer);
        assert_eq!(crate::ABI_VERSION, "1.0.0");
        assert_eq!(crate::WIRE_VERSION, 1);
    }
}
