// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::DrasiLib;

#[path = "provider.rs"]
pub(crate) mod provider;

pub(crate) async fn connect(core: &DrasiLib, edges: Vec<EdgeDefinition>) -> Result<()> {
    connect_with_capacity(core, edges, 1).await
}

pub(crate) async fn connect_with_capacity(
    core: &DrasiLib,
    edges: Vec<EdgeDefinition>,
    capacity: usize,
) -> Result<()> {
    let committed = core.desired_configuration()?;
    let mut desired = committed.desired;
    desired
        .topology
        .relationships
        .extend(edges.into_iter().map(|definition| DesiredRelationship {
            definition,
            policy: Default::default(),
            pipe: DesiredPipe::Bounded { capacity },
        }));
    let receipt = core
        .apply_desired_state(
            committed.revision,
            format!("native-query-connect-{}", committed.revision),
            desired,
        )
        .await?;
    let report = core.reconcile_desired_state().await?;
    anyhow::ensure!(
        report.revision == receipt.revision
            && report.applied
            && report.error.is_none()
            && report.resource_errors.is_empty(),
        "{report:?}"
    );
    Ok(())
}
