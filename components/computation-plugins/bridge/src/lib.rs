// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native data links between complete, independently hosted DrasiLib instances.
//! No component method invocation or process supervision crosses this protocol.

mod boundary;
mod client;
mod codec;
mod config;
mod endpoint_control;
mod error;
mod factory;
mod payload;
mod protocol;
mod receiver;
mod sink;
mod source;
mod state;
mod tls_recipe;
pub mod transport;

pub use boundary::{
    BridgeEgressControl, BridgeIngressControl, EgressStatus, IngressCut, IngressDrainStatus,
};
pub use client::BridgeControl;
pub use codec::PortableEnvelopeCodec;
pub use config::{BridgeConfig, Limits, Namespace, PortableSchema, Profile, Transport};
pub use endpoint_control::BridgeEndpointControl;
pub use error::{BridgeError, ErrorCode, Result};
pub use protocol::{AcceptanceBoundary, LinkStatus, Receipt, PROTOCOL_VERSION};
pub use sink::BridgeSink;
pub use source::BridgeSource;
pub use tls_recipe::{TlsRecipeKind, TlsResourceRecipe, TlsRole};
pub use transport::TlsResource;

pub const SOURCE_FACTORY: &str = "drasi.bridge/source";
pub const SINK_FACTORY: &str = "drasi.bridge/sink";

/// Register these alongside all other normal native factories in every host.
/// Discovery is pure; resources and child-only plugins remain owned by that host.
pub fn factories() -> Vec<std::sync::Arc<dyn drasi_lib::computation::v1::ComponentFactory>> {
    vec![
        std::sync::Arc::new(factory::BridgeFactory::new(true)),
        std::sync::Arc::new(factory::BridgeFactory::new(false)),
    ]
}

#[cfg(test)]
mod tests;
