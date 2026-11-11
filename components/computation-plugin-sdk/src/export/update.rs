// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{ApplyUpdateRequest, ComponentUpdateMode, ComponentUpdatePlan, UpdateRequest};

fn validate_request(state: &FactoryState, request: &UpdateRequest) -> anyhow::Result<()> {
    let current = &request.current;
    let proposed = &request.proposed;
    anyhow::ensure!(
        current.update_directive.is_none() && proposed.update_directive.is_none(),
        "native planning descriptions cannot contain reconstruction execution directives"
    );
    anyhow::ensure!(
        current.id == proposed.id
            && current.scope == proposed.scope
            && current.implementation == proposed.implementation
            && current.configuration_version == proposed.configuration_version
            && current.activation_credentials_bound == proposed.activation_credentials_bound,
        "native update must retain UUID, scope, implementation and configuration version"
    );
    for request in [current, proposed] {
        validate_create_request(state, request)?;
        let metadata = state.plugin.definition.metadata.factories[state.index]
            .for_configuration(&request.configuration)?;
        anyhow::ensure!(
            metadata.capabilities.binds_inputs || request.inputs.is_empty(),
            "native update supplies undeclared construction-time input bindings"
        );
        let mut bindings = std::collections::BTreeSet::new();
        for binding in &request.inputs {
            anyhow::ensure!(
                metadata.ports.iter().any(|port| {
                    port.direction() == PortDirection::Input && port.id() == &binding.input_port
                }),
                "native update binding names an undeclared input port"
            );
            anyhow::ensure!(
                bindings.insert((
                    binding.input_port.clone(),
                    binding.output.clone(),
                    binding.stream.clone(),
                )),
                "native update contains a duplicate construction-time input binding"
            );
        }
    }
    Ok(())
}

pub(super) fn plan(
    state: &FactoryState,
    request: &UpdateRequest,
) -> anyhow::Result<ComponentUpdatePlan> {
    validate_request(state, request)?;
    let factory = &state.plugin.definition.factories[state.index];
    let metadata = &state.plugin.definition.metadata.factories[state.index];
    anyhow::ensure!(
        factory.supports_update_planning() && !metadata.capabilities.update_modes.is_empty(),
        Failure::new(
            abi::status::UNSUPPORTED,
            "native factory does not support updates"
        )
    );
    // Planning has the same description-only view over JSON and during apply
    // validation. Creation annotations cannot change a previously returned plan.
    let mut description = request.clone();
    description.current.activation_credentials_bound = false;
    description.proposed.activation_credentials_bound = false;
    let plan = factory.plan_update(&description)?;
    anyhow::ensure!(
        metadata.capabilities.update_modes.contains(&plan.mode),
        "native planner returned an undeclared update mode"
    );
    if plan.mode != ComponentUpdateMode::Recreate {
        let current = metadata.for_configuration(&request.current.configuration)?;
        let proposed = metadata.for_configuration(&request.proposed.configuration)?;
        anyhow::ensure!(
            current.ports == proposed.ports
                && current.capabilities == proposed.capabilities
                && request.current.inputs == request.proposed.inputs,
            "retained native updates cannot replace fixed ports, capabilities or construction bindings"
        );
    }
    Ok(plan)
}

pub(super) fn validate_apply(
    instance: &Instance,
    update: &mut ApplyUpdateRequest,
) -> anyhow::Result<()> {
    drasi_computation_contracts::validate_identifier(
        "native update operation",
        &update.operation_id,
    )?;
    anyhow::ensure!(
        update.plan.mode != ComponentUpdateMode::Recreate,
        Failure::new(
            abi::status::UNSUPPORTED,
            "Recreate requires retirement and factory construction, not instance apply_update",
        )
    );
    // This local annotation is bound by the creation envelope, never by update
    // wire data. An update cannot rebind the instance's credential service.
    update.request.current.activation_credentials_bound = instance.activation_credentials_bound;
    update.request.proposed.activation_credentials_bound = instance.activation_credentials_bound;
    anyhow::ensure!(
        plan(&instance.factory_state, &update.request)? == update.plan,
        "native apply_update does not match the factory's approved plan"
    );
    Ok(())
}

pub(super) unsafe extern "C" fn plan_update(raw: *mut c_void, input: BorrowedBytes) -> Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state_ref::<FactoryState>(raw)? };
        let bytes = unsafe { transport::borrowed_bytes(input, abi::MAX_METADATA_BYTES)? };
        let request: UpdateRequest = serde_json::from_slice(bytes).map_err(|error| {
            Failure::protocol(format!("invalid native update request: {error}"))
        })?;
        let plan = plan(state, &request).map_err(Failure::from)?;
        let bytes = serde_json::to_vec(&plan)
            .map_err(anyhow::Error::from)
            .map_err(Failure::from)?;
        if bytes.len() > abi::MAX_METADATA_BYTES {
            return Err(Failure::protocol("native update plan exceeds size limit"));
        }
        Ok(bytes)
    })
}
