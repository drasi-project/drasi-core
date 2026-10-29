// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Shared native bridge protocol, transport, controls and recovery state.
//! Component implementations live in the individual source and sink crates.

mod boundary;
#[doc(hidden)]
pub mod client;
#[doc(hidden)]
pub mod codec;
#[doc(hidden)]
pub mod config;
mod endpoint_control;
mod error;
#[doc(hidden)]
pub mod factory;
mod payload;
#[doc(hidden)]
pub mod protocol;
#[doc(hidden)]
pub mod receiver;
#[doc(hidden)]
pub mod state;
mod tls_recipe;
pub mod transport;

pub use boundary::{
    BridgeEgressControl, BridgeIngressControl, EgressStatus, IngressCut, IngressDrainStatus,
};
pub use client::BridgeControl;
pub use codec::PortableEnvelopeCodec;
pub use config::{BridgeConfig, Limits, Namespace, PortableDelivery, Profile, Transport};
pub use endpoint_control::BridgeEndpointControl;
pub use error::{BridgeError, ErrorCode, Result};
pub use protocol::{AcceptanceBoundary, LinkStatus, Receipt, PROTOCOL_VERSION};
pub use tls_recipe::{TlsRecipeKind, TlsResourceRecipe, TlsRole};
pub use transport::TlsResource;

pub const SOURCE_FACTORY: &str = "drasi.bridge/source";
pub const SINK_FACTORY: &str = "drasi.bridge/sink";
