// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use anyhow::Result;
use drasi_lib::{computation::v1::*, DrasiLib};

pub struct InstalledComponents {
    control: GraphControl,
    ids: Vec<ComponentId>,
}

impl InstalledComponents {
    pub async fn add(core: &DrasiLib, batch: ComponentBatch) -> Result<Self> {
        let ids = batch
            .definition
            .components
            .iter()
            .map(|node| node.descriptor.id().clone())
            .collect();
        let report = core.add_components(batch).await?;
        anyhow::ensure!(
            report.committed,
            "component declaration was not committed: {report:?}"
        );
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
        if !exhausted.is_empty() {
            let preview = self
                .control
                .preview(
                    self.control.desired_snapshot().revision,
                    vec![DesiredMutation::Restart(GraphSelection::Exact(exhausted))],
                )
                .await?;
            let report = self
                .control
                .reconcile(preview, TopologyBindings::default())
                .await?;
            anyhow::ensure!(
                report.summary == OperationSummary::Completed,
                "component restart failed: {report:?}"
            );
        }
        Ok(self
            .control
            .start_requested(
                self.control.desired_snapshot().revision,
                GraphSelection::Exact(self.ids.clone()),
            )
            .await?)
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
