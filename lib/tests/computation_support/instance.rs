// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, sync::Arc};

use anyhow::Result;
use drasi_lib::{computation::v1::*, DrasiLib};

pub struct InstalledComponents {
    control: GraphControl,
    ids: Vec<ComponentId>,
}

impl InstalledComponents {
    pub async fn add(core: &DrasiLib, batch: ComponentBatch) -> Result<Self> {
        let ids: Vec<_> = batch
            .definition
            .components
            .iter()
            .map(|node| node.descriptor.id().clone())
            .collect();
        core.add_components(batch).await?;
        let status = core.reconcile_desired_state().await?;
        anyhow::ensure!(status.applied && status.error.is_none(), "{status:?}");
        for id in &ids {
            core.component_by_id(id)?.wait_created().await?;
        }
        Ok(Self {
            control: core.computation_control()?,
            ids,
        })
    }

    pub fn control(&self) -> GraphControl {
        self.control.clone()
    }

    pub fn observed(&self) -> Arc<ObservedGraph> {
        self.control.observed()
    }

    pub async fn start(&self) -> Result<StartReport> {
        let exhausted = self
            .control
            .observed()
            .components
            .iter()
            .filter(|(id, node)| self.ids.contains(id) && node.exhausted)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        anyhow::ensure!(
            exhausted.is_empty(),
            "this managed fixture requires non-finite components; exhausted: {exhausted:?}"
        );
        let members = self
            .ids
            .iter()
            .map(|id| {
                let handle = self.control.component_handle_by_id(id)?;
                Ok((id.clone(), handle.generation()))
            })
            .collect::<GraphResult<BTreeMap<_, _>>>()?;
        Ok(self.control.start_members(members).await?)
    }

    pub async fn stop(&self) -> Result<StopReport> {
        let revision = self.control.desired_snapshot().revision;
        let quiesced = self
            .control
            .quiesce_components(revision, GraphSelection::Exact(self.ids.clone()))
            .await;
        let report = self
            .control
            .stop_components(revision, GraphSelection::Exact(self.ids.clone()))
            .await?;
        quiesced?;
        Ok(report)
    }
}

pub fn instance_graph_id() -> String {
    drasi_lib::management::DesiredInstance::default()
        .topology
        .graph_id
}
