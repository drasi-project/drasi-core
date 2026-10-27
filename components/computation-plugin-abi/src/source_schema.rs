// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional source data-schema inspection. No SQL or mutable database operation
//! is exposed, and reads do not take the component's processing operation slot.

use crate::*;

pub const VERSION: u32 = 1;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_source_schema_v1\0";
pub const MAX_SCHEMA_BYTES: usize = 1024 * 1024;
pub type SourceSchemaFn = unsafe extern "C" fn() -> *const PluginSourceSchemaV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginSourceSchemaV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    /// Returns the instance's immutable support declaration as MessagePack.
    pub supported: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    /// Returns bounded MessagePack Option<SourceSchema>. Lifecycle changes fence
    /// the result; unsupported and retired reads are explicit errors.
    pub begin: Option<unsafe extern "C" fn(*mut c_void, *mut OperationHandle) -> Status>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_extension_preserves_base_layouts() {
        assert_eq!(
            size_of::<PluginSourceSchemaV1>(),
            24 + 2 * size_of::<usize>()
        );
        assert_eq!(size_of::<ComponentVTable>(), 16 + 5 * size_of::<usize>());
        assert_eq!(ABI_VERSION, "1.0.0");
        assert_eq!(WIRE_VERSION, 1);
    }
}
