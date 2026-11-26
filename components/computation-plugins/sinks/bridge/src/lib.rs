// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bridge sink plugin (`drasi.bridge/sink`): the sending endpoint of an
//! inter-instance bridge link. Each input is one admission at the receiver.
//! A durable sender keeps its sender mapping in a synced file it owns under
//! `mappingDirectory`, so a replayed input resolves to the same receiver
//! sequence and is never admitted twice.

mod component;

use drasi_computation_bridge_common::{config, definition};
use drasi_computation_contracts::{ComponentRole, GraphChangeCodec};
use drasi_computation_plugin_sdk::{
    wire::DescribeRequest, Capabilities, Component, ComponentDescription, ConfigDetails,
    ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata, PluginDefinition,
    ServiceBindings,
};
use std::sync::Arc;

pub use component::BridgeSink;
pub use drasi_computation_bridge_common::{
    AcceptanceBoundary, BridgeConfig, BridgeControl, BridgeEndpointControl, BridgeError, ErrorCode,
    IngressCut, Limits, LinkStatus, Namespace, PortableDelivery, PortableEnvelopeCodec, Profile,
    Receipt, TlsRecipeKind, TlsResourceRecipe, TlsRole, Transport, SINK_FACTORY,
};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");

/// Factory for `drasi.bridge/sink`.
pub struct BridgeSinkFactory;

/// The plugin definition, linked statically or exported by the library.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(BridgeSinkFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for BridgeSinkFactory {
    /// A durable sender keys its mapping on the producer's restart-stable
    /// identity and requires exactly-once input; a volatile sender is
    /// unkeyed.
    fn describe(&self, request: &DescribeRequest) -> anyhow::Result<ComponentDescription> {
        let config = definition::parse(&request.request, ComponentRole::Sink)?;
        Ok(config::sender_description(&request.descriptor, &config))
    }

    fn configuration_details(&self) -> ConfigDetails {
        definition::definition(ComponentRole::Sink).details()
    }

    fn metadata(&self) -> FactoryMetadata {
        definition::metadata(
            ComponentRole::Sink,
            Capabilities {
                // A durable sender removes its own mapping file.
                erases_owned_state: true,
                ..Capabilities::default()
            },
        )
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        _services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        let config = definition::parse(request, ComponentRole::Sink)?;
        let scope = request
            .scope
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("the bridge sink requires its graph scope"))?;
        Ok(Component::Sink(Box::new(BridgeSink::new(config, scope.graph_id.clone())?)).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_definition_is_valid() {
        plugin().expect("valid bridge sink plugin");
    }
}
