// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Plugin-authored update intent. The graph must validate the actual lifecycle,
//! storage and input/output requirements before executing a plan.

use serde::{Deserialize, Serialize};

/// How a supported update changes the current component instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ComponentUpdateMode {
    /// Pause processing and update the same activated object.
    InPlace,
    /// Stop, update and start the same initialized object.
    Restart,
    /// Retire the old object and construct a new object with the same logical UUID.
    Recreate,
}

/// What happens to component-owned processing state during an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ComponentUpdateData {
    /// Preserve state that the component author has determined remains compatible.
    Preserve,
    /// Rebuild state from an authoritative bootstrap with a consistent live handover.
    Rebootstrap,
}

/// A component author's proposed update strategy, not permission to discard work.
///
/// The configuration owner must reject a plan whose required data boundaries or
/// downstream reset semantics cannot be established. Neither this plan nor
/// reconstruction permits deleting another component's state or shared history.
/// Unsupported changes are errors, never an implicit default plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentUpdatePlan {
    /// Whether to retain, restart or reconstruct the current object.
    pub mode: ComponentUpdateMode,
    /// Whether the current processing state is reusable.
    pub data: ComponentUpdateData,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_plans_roundtrip_every_explicit_strategy() -> anyhow::Result<()> {
        for mode in [
            ComponentUpdateMode::InPlace,
            ComponentUpdateMode::Restart,
            ComponentUpdateMode::Recreate,
        ] {
            for data in [
                ComponentUpdateData::Preserve,
                ComponentUpdateData::Rebootstrap,
            ] {
                let plan = ComponentUpdatePlan { mode, data };
                let wire = serde_json::to_value(plan)?;
                assert_eq!(serde_json::from_value::<ComponentUpdatePlan>(wire)?, plan);
            }
        }
        assert_eq!(
            serde_json::to_value(ComponentUpdatePlan {
                mode: ComponentUpdateMode::InPlace,
                data: ComponentUpdateData::Preserve,
            })?,
            serde_json::json!({"mode": "inPlace", "data": "preserve"})
        );
        Ok(())
    }

    #[test]
    fn incomplete_or_unknown_plans_do_not_imply_support() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"mode": "restart"}),
            serde_json::json!({"mode": "replace", "data": "preserve"}),
            serde_json::json!({"mode": "recreate", "data": "discardEverything"}),
            serde_json::json!({"mode": "restart", "data": "preserve", "force": true}),
        ] {
            assert!(serde_json::from_value::<ComponentUpdatePlan>(value).is_err());
        }
    }
}
