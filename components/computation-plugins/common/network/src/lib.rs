// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub mod batching;
pub mod config;
#[cfg(feature = "ingress")]
pub mod durable;
#[cfg(feature = "ingress")]
pub mod ingress;
pub mod wire;

pub mod proto {
    #[cfg(feature = "grpc-sink")]
    pub use drasi_grpc_common::reaction;
    #[cfg(feature = "grpc-source")]
    pub use drasi_grpc_common::source;
}
