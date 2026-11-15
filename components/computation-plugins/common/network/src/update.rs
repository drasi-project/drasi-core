// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Update-planning helpers for the network sources.
//!
//! Each source classifies every configuration field by the weakest boundary
//! at which a change can be applied safely. The planner takes the strongest
//! boundary required by any changed field.

use drasi_computation_contracts::ComponentUpdateData;
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    UpdateRequest,
};

/// Accumulates the strongest boundary required by the changed fields.
#[derive(Debug, Default)]
pub struct BoundaryPlanner {
    required: Option<UpdateBoundary>,
    changed: Vec<&'static str>,
}

impl BoundaryPlanner {
    /// Records `field` as requiring `boundary` when `changed` is true.
    pub fn field(&mut self, field: &'static str, changed: bool, boundary: UpdateBoundary) {
        if changed {
            self.changed.push(field);
            self.required = Some(
                self.required
                    .map_or(boundary, |current| current.max(boundary)),
            );
        }
    }

    /// Names of the changed fields, in classification order.
    pub fn changed(&self) -> &[&'static str] {
        &self.changed
    }

    /// The assessment for the recorded changes. Network sources own no
    /// processing state, so every supported change preserves data. An empty
    /// change set is applied in place.
    pub fn assessment(&self) -> Assessment {
        Assessment::supported(
            self.required.unwrap_or(UpdateBoundary::Pause),
            ComponentUpdateData::Preserve,
        )
    }
}

/// Rejects requests that change the component identity, implementation or
/// configuration version; those are different components, not updates.
pub fn ensure_same_component(request: &UpdateRequest) -> anyhow::Result<()> {
    anyhow::ensure!(
        request.current.id == request.proposed.id
            && request.current.implementation == request.proposed.implementation
            && request.current.configuration_version == request.proposed.configuration_version,
        "updates cannot change component identity, implementation or configuration version"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strongest_changed_boundary_wins() {
        let mut planner = BoundaryPlanner::default();
        planner.field("a", true, UpdateBoundary::Pause);
        planner.field("b", false, UpdateBoundary::Replace);
        planner.field("c", true, UpdateBoundary::Stop);
        assert_eq!(planner.changed(), ["a", "c"]);
        assert_eq!(
            planner.assessment(),
            Assessment::supported(UpdateBoundary::Stop, ComponentUpdateData::Preserve)
        );
        assert_eq!(
            BoundaryPlanner::default().assessment(),
            Assessment::supported(UpdateBoundary::Pause, ComponentUpdateData::Preserve)
        );
    }
}
