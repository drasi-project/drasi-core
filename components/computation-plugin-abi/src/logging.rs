// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional host logger. A dynamically loaded library has its own `log`
//! facade state with no logger installed, so the host hands each library a
//! callback once, right after loading it, and the plugin SDK installs a
//! forwarding logger. Records then reach the host's logger with their level,
//! target and message. A library that does not export the symbol simply has
//! no host logger.

use super::*;

pub const SYMBOL: &[u8] = b"drasi_computation_plugin_logging_v1\0";
pub const VERSION: u32 = 1;
/// Installs the borrowed table; the plugin copies it before returning.
pub type InstallFn = unsafe extern "C" fn(*const HostLoggerV1) -> Status;

/// Record levels, ordered like the `log` facade. `OFF` disables forwarding.
pub mod level {
    pub const OFF: u32 = 0;
    pub const ERROR: u32 = 1;
    pub const WARN: u32 = 2;
    pub const INFO: u32 = 3;
    pub const DEBUG: u32 = 4;
    pub const TRACE: u32 = 5;
}

/// Host-owned and valid for the process lifetime (libraries are pinned).
/// `log` is synchronous, bounded and callable from any thread; it never
/// waits for I/O capacity on the plugin's behalf and must contain panics.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostLoggerV1 {
    pub header: Header,
    pub version: u32,
    /// The most verbose level the host records when the library is loaded.
    pub max_level: u32,
    pub context: *mut c_void,
    /// Level, UTF-8 target and UTF-8 message, each borrowed for the call.
    pub log: Option<unsafe extern "C" fn(*mut c_void, u32, BorrowedBytes, BorrowedBytes)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_logger_table_layout_is_frozen() {
        let pointer = size_of::<usize>();
        assert_eq!(size_of::<HostLoggerV1>(), 24 + 2 * pointer);
    }
}
