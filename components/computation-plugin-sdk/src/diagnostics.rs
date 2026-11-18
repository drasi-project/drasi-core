// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Operator-visible output from native plugins.
//!
//! Plugins write diagnostics and operator output with the [`log`] facade
//! (re-exported as `drasi_computation_plugin_sdk::log`). Statically linked,
//! the plugin shares the host's facade. Loaded as a library, the plugin has
//! its own facade state; the exported library installs a logger that forwards
//! every enabled record to the host's logger through the ABI's
//! [`abi::logging`] table, so the same `log::info!` reaches the host either
//! way. A host that does not install a logger leaves the library's facade
//! disabled, exactly as an application without a logger would.

use crate::{abi, transport::Failure};
use std::sync::OnceLock;

struct HostLogger(abi::logging::HostLoggerV1);

// The table is host-owned for the process lifetime and its callback is
// callable from any thread.
unsafe impl Send for HostLogger {}
unsafe impl Sync for HostLogger {}

static HOST_LOGGER: OnceLock<HostLogger> = OnceLock::new();

impl log::Log for HostLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let Some(callback) = self.0.log else {
            return;
        };
        let message = record.args().to_string();
        let target = record.target();
        // Safety: the host guarantees the callback and its context for the
        // process lifetime; both byte strings are borrowed for the call.
        unsafe {
            callback(
                self.0.context,
                level_code(record.level()),
                abi::BorrowedBytes::new(target.as_bytes()),
                abi::BorrowedBytes::new(message.as_bytes()),
            )
        };
    }

    fn flush(&self) {}
}

fn level_code(level: log::Level) -> u32 {
    match level {
        log::Level::Error => abi::logging::level::ERROR,
        log::Level::Warn => abi::logging::level::WARN,
        log::Level::Info => abi::logging::level::INFO,
        log::Level::Debug => abi::logging::level::DEBUG,
        log::Level::Trace => abi::logging::level::TRACE,
    }
}

fn level_filter(code: u32) -> log::LevelFilter {
    match code {
        abi::logging::level::ERROR => log::LevelFilter::Error,
        abi::logging::level::WARN => log::LevelFilter::Warn,
        abi::logging::level::INFO => log::LevelFilter::Info,
        abi::logging::level::DEBUG => log::LevelFilter::Debug,
        code if code >= abi::logging::level::TRACE => log::LevelFilter::Trace,
        _ => log::LevelFilter::Off,
    }
}

/// Installs the host's logger as this library's `log` logger. Called once by
/// the host through the exported `drasi_computation_plugin_logging_v1`; a
/// repeated call keeps the first table.
///
/// # Safety
/// `table` must point to a valid [`abi::logging::HostLoggerV1`] that, with its
/// context, stays valid for the process lifetime.
#[doc(hidden)]
pub unsafe fn install_host_logger(table: *const abi::logging::HostLoggerV1) -> Result<(), Failure> {
    let table = unsafe { table.as_ref() }
        .copied()
        .ok_or_else(|| Failure::protocol("missing host logger table"))?;
    table
        .header
        .validate::<abi::logging::HostLoggerV1>()
        .map_err(|error| Failure::protocol(error.to_string()))?;
    if table.version != abi::logging::VERSION || table.log.is_none() {
        return Err(Failure::protocol("unsupported host logger table"));
    }
    let mut installed = false;
    let logger = HOST_LOGGER.get_or_init(|| {
        installed = true;
        HostLogger(table)
    });
    if installed && log::set_logger(logger).is_ok() {
        log::set_max_level(level_filter(table.max_level));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_codes_round_trip_through_the_host_table() {
        for level in [
            log::Level::Error,
            log::Level::Warn,
            log::Level::Info,
            log::Level::Debug,
            log::Level::Trace,
        ] {
            assert_eq!(level_filter(level_code(level)), level.to_level_filter());
        }
        assert_eq!(
            level_filter(abi::logging::level::OFF),
            log::LevelFilter::Off
        );
    }
}
