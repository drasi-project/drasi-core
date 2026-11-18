// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The host side of the native logging extension: records a loaded library
//! writes through its `log` facade reach this process's logger.

use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::transport::take_status;

struct Table(abi::logging::HostLoggerV1);
// The table has no context and a thread-safe callback.
unsafe impl Sync for Table {}

/// Gives a freshly loaded library the host logger. The table must outlive
/// the library, which is pinned for the process lifetime, so it is built
/// once per load with the host's level at that time and leaked.
pub(super) fn install(entry: abi::logging::InstallFn) -> anyhow::Result<()> {
    let table: &'static Table = Box::leak(Box::new(Table(abi::logging::HostLoggerV1 {
        header: abi::Header::new::<abi::logging::HostLoggerV1>(),
        version: abi::logging::VERSION,
        max_level: level_code(log::max_level()),
        context: std::ptr::null_mut(),
        log: Some(forward),
    })));
    unsafe { take_status(entry(&table.0)) }
        .map_err(|failure| anyhow::anyhow!("native logging extension refused: {failure}"))
}

fn level_code(filter: log::LevelFilter) -> u32 {
    match filter {
        log::LevelFilter::Off => abi::logging::level::OFF,
        log::LevelFilter::Error => abi::logging::level::ERROR,
        log::LevelFilter::Warn => abi::logging::level::WARN,
        log::LevelFilter::Info => abi::logging::level::INFO,
        log::LevelFilter::Debug => abi::logging::level::DEBUG,
        log::LevelFilter::Trace => abi::logging::level::TRACE,
    }
}

unsafe extern "C" fn forward(
    _context: *mut std::ffi::c_void,
    level: u32,
    target: abi::BorrowedBytes,
    message: abi::BorrowedBytes,
) {
    let _ = std::panic::catch_unwind(|| {
        let level = match level {
            abi::logging::level::ERROR => log::Level::Error,
            abi::logging::level::WARN => log::Level::Warn,
            abi::logging::level::INFO => log::Level::Info,
            abi::logging::level::DEBUG => log::Level::Debug,
            abi::logging::level::TRACE => log::Level::Trace,
            _ => return,
        };
        let text = |bytes: abi::BorrowedBytes| {
            if bytes.len == 0 || bytes.data.is_null() {
                return std::borrow::Cow::Borrowed("");
            }
            // Safety: the plugin borrows `len` readable bytes for this call.
            String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(bytes.data, bytes.len) })
        };
        let target = text(target);
        let message = text(message);
        log::logger().log(
            &log::Record::builder()
                .level(level)
                .target(&target)
                .args(format_args!("{message}"))
                .build(),
        );
    });
}
