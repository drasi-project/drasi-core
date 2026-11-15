// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Shared helpers for the native network components: ingress configuration,
//! security (authentication, rate limiting, TLS material), the source ingress
//! runtime with pause/stop control, durable admission and wire conversion.
//!
//! This crate registers no component factories. Each component crate owns its
//! configuration type, factory, transport and export.

#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub mod batching;
pub mod config;
#[cfg(feature = "ingress")]
pub mod durable;
#[cfg(feature = "ingress")]
pub mod ingress;
#[cfg(feature = "ingress")]
pub mod security;
#[cfg(feature = "ingress")]
pub mod update;
pub mod wire;

#[doc(hidden)]
pub use log;

pub mod proto {
    #[cfg(feature = "grpc-sink")]
    pub use drasi_grpc_common::reaction;
    #[cfg(feature = "grpc-source")]
    pub use drasi_grpc_common::source;
}

/// Logs through the `log` facade when the host installed a logger for this
/// library (statically linked hosts). A dynamically loaded library has its own
/// `log` state with no logger, so warnings and errors fall back to standard
/// error and lower levels are dropped instead of flooding it.
#[macro_export]
macro_rules! diag {
    ($level:ident, $($arg:tt)+) => {{
        let level = $crate::log::Level::$level;
        if level <= $crate::log::max_level() {
            $crate::log::log!(target: "drasi.network", level, $($arg)+);
        } else if level <= $crate::log::Level::Warn {
            eprintln!("drasi.network {}: {}", level, format_args!($($arg)+));
        }
    }};
}
