// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bridge source plugin (`drasi.bridge/source`): the receiving endpoint of an
//! inter-instance bridge link. A volatile link admits frames into a bounded
//! queue drained through `next`; a durable link admits them through the
//! Host's source admission service into a dedicated admission journal.
//!
//! Seal and drain are served on the link's own control lane
//! (`BridgeEndpointControl`), not through a Host resource.

mod component;

use drasi_computation_bridge_common::{config, definition};
use drasi_computation_contracts::{ComponentRole, GraphChangeCodec};
use drasi_computation_plugin_sdk::{
    wire::DescribeRequest, Capabilities, Component, ComponentDescription, ConfigDetails,
    ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata, HostServices,
    PluginDefinition, ServiceBindings,
};
use std::sync::Arc;

pub use component::BridgeSource;
pub use drasi_computation_bridge_common::{
    BridgeConfig, BridgeEndpointControl, BridgeError, ErrorCode, IngressCut, IngressDrainStatus,
    Limits, LinkStatus, Namespace, PortableDelivery, Profile, TlsRecipeKind, TlsResourceRecipe,
    TlsRole, Transport, SOURCE_FACTORY,
};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");

/// Factory for `drasi.bridge/source`.
pub struct BridgeSourceFactory;

/// The plugin definition, linked statically or exported by the library.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(BridgeSourceFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for BridgeSourceFactory {
    /// A durable link publishes through its required durable admission
    /// journal under a restart-stable producer identity; a volatile link
    /// under a new volatile identity on every construction.
    fn describe(&self, request: &DescribeRequest) -> anyhow::Result<ComponentDescription> {
        let config = definition::parse(&request.request, ComponentRole::Source)?;
        Ok(config::receiver_description(
            &request.descriptor,
            config.profile,
        ))
    }

    fn configuration_details(&self) -> ConfigDetails {
        definition::definition(ComponentRole::Source).details()
    }

    fn metadata(&self) -> FactoryMetadata {
        definition::metadata(
            ComponentRole::Source,
            Capabilities {
                services: HostServices {
                    source_admission: true,
                    ..Default::default()
                },
                ..Capabilities::default()
            },
        )
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        let config = definition::parse(request, ComponentRole::Source)?;
        let scope = request
            .scope
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("the bridge source requires its graph scope"))?;
        Ok(Component::Source(Box::new(BridgeSource::new(
            config,
            scope.graph_id.clone(),
            services.admission,
        )?))
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_definition_is_valid() {
        plugin().expect("valid bridge source plugin");
    }
}
