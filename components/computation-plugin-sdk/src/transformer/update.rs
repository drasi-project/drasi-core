// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The update rules every stateful transformer shares. An implementation
//! decides only whether its own settings can change in place.

use anyhow::Result;
use drasi_computation_contracts::{ComponentUpdateData, ComponentUpdateMode, ComponentUpdatePlan};

use crate::wire::{CreateRequest, UpdateRequest};

/// The shared options that select the storage profile, initialization and
/// recovery. No update changes them.
const FIXED_OPTIONS: [&str; 5] = [
    "volatile",
    "publication",
    "bootstrap",
    "recovery",
    "resetConfiguration",
];

fn reconstruction(request: &CreateRequest) -> ComponentUpdatePlan {
    ComponentUpdatePlan {
        mode: ComponentUpdateMode::Recreate,
        data: if request.configuration.get("bootstrap") == Some(&serde_json::Value::Bool(true)) {
            ComponentUpdateData::Rebootstrap
        } else {
            ComponentUpdateData::Reset
        },
    }
}

/// The plan for an update, given whether the implementation can apply its
/// configuration change to the running transformer (`in_place`). An in-place
/// update keeps the state, so it may only increase output retention.
/// Otherwise the transformer is reconstructed: it initializes from upstream
/// again when `bootstrap` is configured, and starts empty when it is not.
pub fn update_plan(request: &UpdateRequest, in_place: bool) -> Result<ComponentUpdatePlan> {
    for field in FIXED_OPTIONS {
        anyhow::ensure!(
            request.current.configuration.get(field) == request.proposed.configuration.get(field),
            "updates cannot change the storage, bootstrap or recovery policy"
        );
    }
    if !in_place {
        return Ok(reconstruction(&request.proposed));
    }
    anyhow::ensure!(
        super::output_capacity(&request.proposed)? >= super::output_capacity(&request.current)?,
        "retained updates cannot discard history by reducing output retention"
    );
    Ok(ComponentUpdatePlan {
        mode: ComponentUpdateMode::InPlace,
        data: ComponentUpdateData::Preserve,
    })
}

/// Refuse a reconstruction directive whose data policy disagrees with the
/// configured startup: a bootstrap, or an empty start.
pub fn validate_reconstruction(request: &CreateRequest) -> Result<()> {
    if let Some(directive) = &request.update_directive {
        directive.validate()?;
        anyhow::ensure!(
            directive.plan.data == reconstruction(request).data,
            "reconstruction must match the configured bootstrap or empty startup"
        );
    }
    Ok(())
}
