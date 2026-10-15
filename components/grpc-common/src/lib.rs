// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Shared gRPC wire contracts, with no component implementations or plugin SDK.
//! Source and reaction contracts use separate modules because their historical
//! `drasi.v1.QueryResult` messages have different layouts.

mod value;
pub use value::{convert_json_to_proto_struct, convert_json_to_proto_value};

/// Include root for build scripts importing the shared source `.proto` files.
/// This is a build-time package path, not a runtime installation location.
pub const SOURCE_PROTO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/proto/source");

/// Source events, bootstrap messages and source-service client/server bindings.
#[cfg(feature = "source")]
#[allow(clippy::unwrap_used)]
pub mod source {
    include!(concat!(env!("OUT_DIR"), "/source/drasi.v1.rs"));
}

/// Reaction messages and reaction-service client/server bindings.
#[cfg(feature = "reaction")]
#[allow(clippy::unwrap_used)]
pub mod reaction {
    include!(concat!(env!("OUT_DIR"), "/reaction/drasi.v1.rs"));
}

#[cfg(test)]
mod tests;
