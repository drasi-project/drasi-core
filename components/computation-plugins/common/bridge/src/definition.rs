// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The published configuration of the bridge source and sink factories, and
//! the checked parse every factory entry point shares.

use drasi_computation_contracts::{ComponentRole, ImplementationIdentity, PortDescriptor};
use drasi_computation_plugin_sdk::{
    Capabilities, ConfigDefinition, ConfigOption, ConfigType, CreateRequest, FactoryMetadata,
};
use serde_json::Value;

use crate::{BridgeConfig, Limits, PortableDelivery, SINK_FACTORY, SOURCE_FACTORY};

/// The published options of one endpoint role. The sink alone has
/// `onPermanentFailure` and `mappingDirectory`.
pub fn definition(role: ComponentRole) -> ConfigDefinition {
    use ConfigType::{ComponentId, Object, String};
    let source = role == ComponentRole::Source;
    let mut options = vec![
        ConfigOption::new("linkId", String, true, "Identifier of the link, shared by both endpoints."),
        ConfigOption::new("senderInstance", String, true, "Instance ID of the sending DrasiLib instance."),
        ConfigOption::new("receiverInstance", String, true, "Instance ID of the receiving DrasiLib instance."),
        ConfigOption::new("senderEndpoint", ComponentId, true, "Component UUID of the bridge sink."),
        ConfigOption::new("receiverEndpoint", ComponentId, true, "Component UUID of the bridge source."),
        ConfigOption::new("stream", String, true, "This endpoint's local stream binding."),
        ConfigOption::new("peerStream", String, true, "The other endpoint's stream binding."),
        ConfigOption::new("delivery", String, true, "events, or complete-transactions to require a current complete-transaction marker on every frame.")
            .allowed(["events", "complete-transactions"].map(Value::from)),
        ConfigOption::new("profile", String, true, "volatile (bounded receiver queue) or durable (receiver admission journal and sender mapping file).")
            .allowed(["volatile", "durable"].map(Value::from)),
        ConfigOption::new("failureScope", String, false, "The failure a durable link's commits survive.")
            .default(Value::from("processRestart"))
            .allowed(["processRestart", "powerLoss", "storageLoss"].map(Value::from)),
        ConfigOption::new("namespace", Object, true, "Namespace authority: {version: 1, sourcePrefixes: [...]} over element namespaces."),
        ConfigOption::new("transport", Object, true, "unix {dataPath, controlPath}, namedPipe {dataPipe, controlPipe} or tls {dataAddress, controlAddress, serverName}."),
        ConfigOption::new("limits", Object, false, "Frame, value, queue, timeout and retry limits.")
            .default(serde_json::to_value(Limits::default()).unwrap_or(Value::Null)),
        ConfigOption::new("tls", Object, false, "Mutual-TLS identity (bridgeTls recipe: role, trustRoots, certificateChain, privateKey, peerCertificateSha256); required exactly for the tls transport. Relative paths resolve against the working directory."),
    ];
    if !source {
        options.push(ConfigOption::new(
            "mappingDirectory",
            String,
            false,
            "Directory of the durable sender's mapping file; required exactly for the durable profile. Relative paths resolve against the working directory.",
        ));
        options.push(ConfigOption::on_permanent_failure());
    }
    // Constant declarations; the crates' metadata tests prove they validate.
    ConfigDefinition::try_new(options).expect("constant bridge configuration")
}

/// Factory metadata shared by both endpoints. The bridge source declares
/// source admission; neither endpoint declares management, so every change
/// reconstructs the endpoint.
pub fn metadata(role: ComponentRole, capabilities: Capabilities) -> FactoryMetadata {
    let source = role == ComponentRole::Source;
    let port = if source {
        PortDescriptor::new(
            drasi_computation_contracts::PortId::try_new("out").expect("constant port"),
            drasi_computation_contracts::PortDirection::Output,
            PortableDelivery::Events.schema().descriptor().clone(),
            crate::config::port_requirements(),
        )
    } else {
        PortDescriptor::new(
            drasi_computation_contracts::PortId::try_new("in").expect("constant port"),
            drasi_computation_contracts::PortDirection::Input,
            PortableDelivery::Events.schema().descriptor().clone(),
            crate::config::port_requirements(),
        )
    };
    FactoryMetadata {
        implementation: ImplementationIdentity::try_new(
            if source { SOURCE_FACTORY } else { SINK_FACTORY },
            "1",
        )
        .expect("constant bridge factory identity"),
        role,
        configuration_version: 1,
        configuration: definition(role).schema(),
        ports: vec![port],
        completion: (!source).then_some(drasi_computation_contracts::SinkCompletion::Handled),
        capabilities,
    }
}

/// Parses and validates a construction's configuration for `role`, including
/// the endpoint's instance scope and its configured component UUID.
pub fn parse(request: &CreateRequest, role: ComponentRole) -> anyhow::Result<BridgeConfig> {
    let config: BridgeConfig = serde_json::from_value(request.configuration.clone())?;
    config.validate_role(role)?;
    let source = role == ComponentRole::Source;
    anyhow::ensure!(
        &request.id
            == if source {
                &config.receiver_endpoint
            } else {
                &config.sender_endpoint
            },
        "bridge component UUID differs from its configured endpoint"
    );
    if let Some(scope) = &request.scope {
        anyhow::ensure!(
            scope.instance_id
                == if source {
                    config.receiver_instance.as_str()
                } else {
                    config.sender_instance.as_str()
                },
            "bridge endpoint belongs to a different instance"
        );
        crate::config::identifier(&scope.graph_id)?;
    }
    Ok(config)
}
