// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{
    lifecycle::{ApplyError, Assessment},
    ApplyUpdateRequest, ComponentUpdateMode, UpdateRequest,
};

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

/// Factory planner assessment, validated against the declared boundaries.
pub(super) fn assess_factory(
    state: &FactoryState,
    request: &UpdateRequest,
) -> anyhow::Result<Assessment> {
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
    let assessment = factory.assess(&description(request))?;
    validate_assessment(state, request, &assessment)?;
    Ok(assessment)
}

fn description(request: &UpdateRequest) -> UpdateRequest {
    let mut description = request.clone();
    description.current.activation_credentials_bound = false;
    description.proposed.activation_credentials_bound = false;
    description
}

fn validate_assessment(
    state: &FactoryState,
    request: &UpdateRequest,
    assessment: &Assessment,
) -> anyhow::Result<()> {
    if let Some(plan) = assessment.plan() {
        validate_mode(state, request, plan.mode)?;
    }
    Ok(())
}

fn validate_mode(
    state: &FactoryState,
    request: &UpdateRequest,
    mode: ComponentUpdateMode,
) -> anyhow::Result<()> {
    let metadata = &state.plugin.definition.metadata.factories[state.index];
    anyhow::ensure!(
        metadata.capabilities.update_modes.contains(&mode),
        "native assessment returned an undeclared update boundary"
    );
    if mode != ComponentUpdateMode::Recreate {
        let current = metadata.for_configuration(&request.current.configuration)?;
        let proposed = metadata.for_configuration(&request.proposed.configuration)?;
        anyhow::ensure!(
            current.ports == proposed.ports
                && current.capabilities == proposed.capabilities
                && request.current.inputs == request.proposed.inputs,
            "retained native updates cannot replace fixed ports, capabilities or construction bindings"
        );
    }
    Ok(())
}

/// Live instance assessment through the component's management interface.
pub(super) fn assess_instance(
    instance: &Instance,
    component: &mut Component,
    request: &UpdateRequest,
) -> anyhow::Result<Assessment> {
    let mut request = request.clone();
    request.current.activation_credentials_bound = instance.activation_credentials_bound;
    request.proposed.activation_credentials_bound = instance.activation_credentials_bound;
    validate_request(&instance.factory_state, &request)?;
    let management = component.base_mut().management().ok_or_else(|| {
        Failure::new(
            abi::status::UNSUPPORTED,
            "native component does not export management",
        )
    })?;
    let assessment = management.assess(&description(&request))?;
    validate_assessment(&instance.factory_state, &request, &assessment)?;
    Ok(assessment)
}

/// Structural checks before the boundary is entered. The live re-assessment
/// happens under the component lock when the update executes.
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
            "Replace requires retirement and factory construction, not instance apply",
        )
    );
    // This local annotation is bound by the creation envelope, never by update
    // wire data. An update cannot rebind the instance's credential service.
    update.request.current.activation_credentials_bound = instance.activation_credentials_bound;
    update.request.proposed.activation_credentials_bound = instance.activation_credentials_bound;
    validate_request(&instance.factory_state, &update.request)?;
    validate_mode(&instance.factory_state, &update.request, update.plan.mode)
}

/// Re-assesses the accepted change on the locked instance. A different
/// outcome refuses the update without mutating the object.
pub(super) fn recheck_apply(
    instance: &Instance,
    component: &mut Component,
    update: &ApplyUpdateRequest,
) -> Result<(), ApplyError> {
    let assessment = assess_instance(instance, component, &update.request)?;
    let accepted = Assessment::Supported {
        boundary: update.plan.mode.into(),
        data: update.plan.data,
        fingerprint: update.fingerprint.clone(),
    };
    if assessment == accepted {
        return Ok(());
    }
    Err(ApplyError::Refused {
        now_requires: assessment.plan().map(|plan| plan.mode.into()),
        reason: "the live assessment no longer matches the accepted update".into(),
    })
}

pub(super) unsafe extern "C" fn assess(raw: *mut c_void, input: BorrowedBytes) -> Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state_ref::<FactoryState>(raw)? };
        let request = decode_update_request(input)?;
        encode_assessment(&assess_factory(state, &request).map_err(Failure::from)?)
    })
}

pub(super) unsafe fn decode_update_request(input: BorrowedBytes) -> Result<UpdateRequest, Failure> {
    let bytes = unsafe { transport::borrowed_bytes(input, abi::MAX_METADATA_BYTES)? };
    serde_json::from_slice(bytes)
        .map_err(|error| Failure::protocol(format!("invalid native update request: {error}")))
}

pub(super) fn encode_assessment(assessment: &Assessment) -> Result<Vec<u8>, Failure> {
    let bytes = serde_json::to_vec(assessment)
        .map_err(anyhow::Error::from)
        .map_err(Failure::from)?;
    if bytes.len() > abi::MAX_METADATA_BYTES {
        return Err(Failure::protocol("native assessment exceeds size limit"));
    }
    Ok(bytes)
}
