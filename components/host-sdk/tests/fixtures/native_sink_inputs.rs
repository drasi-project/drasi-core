// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only stand-in for the graph's declared incoming edges. A sink created
//! outside a graph receives one binding per `outputs` alias, carried on the
//! stream `<component>/<port>`.

use anyhow::Result;
use drasi_computation_contracts::{ComponentId, InputBinding, OutputEndpoint, PortId, StreamId};
use drasi_host_sdk::computation::{NativeComponentProxy, NativeFactory};
use serde_json::Value;

#[allow(dead_code)]
pub fn inputs_for(configuration: &Value) -> Vec<InputBinding> {
    let outputs = configuration
        .get("outputs")
        .or_else(|| configuration.pointer("/settings/outputs"))
        .and_then(Value::as_object);
    outputs
        .into_iter()
        .flatten()
        .map(|(_, alias)| {
            let component = alias["component"].as_str().expect("alias component");
            let port = alias["port"].as_str().expect("alias port");
            InputBinding {
                input_port: PortId::try_new("in").expect("port"),
                output: OutputEndpoint::new(
                    ComponentId::try_new(component).expect("component"),
                    PortId::try_new(port).expect("port"),
                ),
                stream: StreamId::try_new(format!("{component}/{port}")).expect("stream"),
            }
        })
        .collect()
}

#[allow(dead_code)]
pub fn create(
    factory: &NativeFactory,
    id: ComponentId,
    configuration: Value,
) -> Result<NativeComponentProxy> {
    let inputs = if factory.metadata().capabilities.binds_inputs {
        inputs_for(&configuration)
    } else {
        Vec::new()
    };
    factory.create_component_with_inputs(id, configuration, inputs)
}

#[allow(dead_code)]
pub fn create_with_identity(
    factory: &NativeFactory,
    id: ComponentId,
    configuration: Value,
    identity: std::sync::Arc<drasi_host_sdk::computation::NativeIdentityResource>,
) -> Result<NativeComponentProxy> {
    let inputs = inputs_for(&configuration);
    factory.create_component_with_identity_and_inputs(id, configuration, identity, inputs)
}
